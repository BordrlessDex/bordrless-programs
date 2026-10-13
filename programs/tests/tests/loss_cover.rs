//! Loss cover end to end (`docs/companions.md` "Loss cover"): a companion made with the Loss cover
//! hook and script, launched from a config naming the hook; holders buy high, the price falls, the
//! pot pays every holder under water the same fraction of its uncovered loss, round by round. The
//! first round measures; a paying round pays `uncovered * b / l_prev` to each; a round whose
//! budget runs out touches the rest, and the next round catches up; a holding is measured or paid
//! once a round; the minimums (holding, loss, age, pot) and the launcher's own holding are skipped;
//! a round after a gap measures only; a partial sell keeps its cover; graduation changes nothing;
//! a sender cannot change `f`; and a step's size and compute as the keeper sends it.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorDeserialize, AnchorSerialize, Discriminator};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayoutTermsArgs};
use bordrless_companion::state::{Companion, PayoutTerms, Script, Split};
use bordrless_core::policy;
use bordrless_game::round_of;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_strategy::PoolReserves;
use bordrless_token::client as token;
use loss_cover_hook::{client as hook, spot_of, Record};
use loss_cover_script::{
    fraction_of, uncovered_loss, CoverRound, CoverState, CoverTerms, LossCoverScriptError,
    STATE_LEN,
};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SCRIPT: Pubkey = loss_cover_script::ID;
const HOOK: Pubkey = loss_cover_hook::ID;
const CREATOR_FEE: u16 = 200;
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
/// Five-minute rounds (`T0` starts one) and 15-second steps.
const ROUND: u32 = 300;
const R: i64 = ROUND as i64;
const STEP: i64 = 15;

fn script_code(e: LossCoverScriptError) -> u32 {
    u32::from(e)
}

/// The Borsh of `t`.
fn borsh<T: AnchorSerialize>(t: &T) -> Vec<u8> {
    let mut out = Vec::new();
    t.serialize(&mut out).unwrap();
    out
}

/// A world with the Loss cover script deployed by Bordrless and approved (no author).
fn world() -> World {
    let mut w = World::new();
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &SCRIPT);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
    w
}

/// Everything covered, no minimums, five-minute rounds, no minimum pot.
fn open_terms() -> CoverTerms {
    CoverTerms {
        cover_bps: 10_000,
        min_holding: 1,
        min_loss: 1,
        min_age_secs: 0,
        round_secs: ROUND,
        min_pot_lamports: 0,
    }
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

/// A Loss cover coin with `terms`, launched by `launcher`: the companion, the hook prepared, the
/// script with its payout terms (every cap at 100%, the window the round), the launch from a
/// config naming the hook. Answers the mint.
fn launch_coin(w: &mut World, launcher: &Keypair, terms: CoverTerms) -> Pubkey {
    let (config, tx) = w.create_config(
        launcher,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(HOOK),
            custom_hook_flags: loss_cover_hook::FLAGS,
            label: "Loss cover".to_string(),
        },
    );
    tx.ok();
    let mint = Keypair::new();
    let m = mint.pubkey();
    let args = CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: SCRIPT,
        interval: STEP,
        state_len: STATE_LEN,
        init_data: borsh(&terms),
    };
    let payout = PayoutTermsArgs {
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: terms.round_secs,
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        hook::prepare(launcher.pubkey(), m),
        companion::create_script_v2(launcher.pubkey(), m, args, payout, vec![]),
    ];
    let tx = w.env.send_paid_by(&ixs, launcher, &[&mint]);
    tx.ok();
    println!(
        "setup (create, prepare, create_script_v2): {} bytes, CU {}",
        tx.size,
        tx.cu()
    );
    let c = w.launch_config(&config);
    let custom = w.custom_hook_accounts(&HOOK, &m);
    let largs = World::launch_args("LCOV", c.creator_fee_bps, VQ, c.rules);
    let inner = launch::create_launch_with(
        companion::creator_address(&m),
        m,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        largs.clone(),
        Some(config),
        Some(&custom),
    );
    let ix = companion::launch(launcher.pubkey(), m, &inner, largs);
    let tx = w.env.send_paid_by(&[ix], launcher, &[&mint]);
    tx.ok();
    assert!(tx.max_height() <= 5);
    assert_eq!(w.launch(&m).custom_hook, Some(HOOK));
    w.env.warp(31);
    m
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn cover_state(w: &World, mint: &Pubkey) -> CoverState {
    w.env.read(&companion::script_state_address(mint))
}

fn terms_of(w: &World, mint: &Pubkey) -> PayoutTerms {
    w.env.read(&companion::payout_terms_address(mint))
}

fn record(w: &World, mint: &Pubkey, owner: &Pubkey) -> Record {
    Record::decode(&w.env.hook_data(mint, owner))
}

fn spot(w: &World, mint: &Pubkey) -> u64 {
    let p = w.launch_pool(mint);
    spot_of(&PoolReserves {
        base_mint: p.base_mint,
        quote_mint: p.quote_mint,
        lp_fee_bps: p.lp_fee_bps,
        base_reserve: p.base_reserve,
        quote_reserve: p.quote_reserve,
        virtual_base: p.virtual_base,
        virtual_quote: p.virtual_quote,
    })
    .unwrap()
}

/// What `owner` is owed now, as the script computes it (0 when not under water).
fn owed(w: &World, mint: &Pubkey, owner: &Pubkey) -> u64 {
    let r = record(w, mint, owner);
    let s = cover_state(w, mint);
    uncovered_loss(r.entry, spot(w, mint), r.amount, s.cover_bps, r.paid)
}

/// A wallet that bought `sol` of the token.
fn holder(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol + SOL);
    w.buy(&t, mint, sol).ok();
    t
}

/// Claims the fees into the pot (70% of them). Answers the cranker.
fn claim(w: &mut World, mint: &Pubkey) -> Keypair {
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    cranker
}

/// Bridged SOL given to the creator's holding and claimed: the pot grows by 70% of it less the
/// claim's bounty (what a claim does with anything that reaches the holding).
fn fund_pot(w: &mut World, mint: &Pubkey, lamports: u64) {
    let donor = w.wallet_with_sol(lamports + SOL);
    let creator = companion::creator_address(mint);
    let sol = w.sol;
    w.env
        .send_paid_by(
            &[token::transfer_with(
                donor.pubkey(),
                token::holding_address(&sol, &donor.pubkey()),
                token::holding_address(&sol, &creator),
                sol,
                None,
                vec![],
                lamports,
            )],
            &donor,
            &[],
        )
        .ok();
    claim(w, mint);
}

/// The step's accounts after the companion's six: the payout accounts, then each candidate's
/// holding (writable: the hook records into it) and, when `paid`, its owner (writable).
fn step_ix(w: &World, cranker: Pubkey, mint: &Pubkey, list: &[(Pubkey, bool)]) -> Instruction {
    let mut extra = companion::payout_accounts(mint, &HOOK);
    for (owner, paid) in list {
        extra.push(AccountMeta::new(token::holding_address(mint, owner), false));
        if *paid {
            extra.push(AccountMeta::new(*owner, false));
        }
    }
    companion::script_step(cranker, &keys(w, mint), SCRIPT, extra)
}

fn step(w: &mut World, cranker: &Keypair, mint: &Pubkey, list: &[(Pubkey, bool)]) -> Tx {
    let ix = step_ix(w, cranker.pubkey(), mint, list);
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// Warps to the first second of the next round.
fn next_round(w: &mut World) {
    let now = w.env.now;
    w.env.warp(R - now % R);
}

/// The current round.
fn round_now(w: &World) -> u32 {
    round_of(w.env.now, ROUND)
}

/// A tiny base64 decoder for `Program data:` log lines.
fn b64(s: &str) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c).expect("base64") << (18 - 6 * i);
        }
        let n = chunk.len() * 6 / 8;
        for i in 0..n {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// The script's `CoverRound` events, from the transaction's logs (`emit!`, not a self-CPI).
fn cover_rounds(tx: &Tx) -> Vec<CoverRound> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .map(b64)
        .filter(|b| b.len() >= 8 && b[..8] == CoverRound::DISCRIMINATOR[..])
        .map(|b| CoverRound::deserialize(&mut &b[8..]).expect("a CoverRound"))
        .collect()
}

/// Holders who bought high: a whale buys, the holders buy, the whale sells half (the rest can
/// fall on the price later). Answers the holders (all under water), the cranker of the first fee
/// claim and the whale.
fn under_water(w: &mut World, mint: &Pubkey, sols: &[u64]) -> (Vec<Keypair>, Keypair, Keypair) {
    let whale = w.wallet_with_sol(40 * SOL);
    w.buy(&whale, mint, 25 * SOL).ok();
    let holders: Vec<Keypair> = sols.iter().map(|s| holder(w, mint, *s)).collect();
    let held = w.env.holding(mint, &whale.pubkey());
    w.sell(&whale, mint, held / 2).ok();
    let cranker = claim(w, mint);
    for h in &holders {
        assert!(owed(w, mint, &h.pubkey()) > 0, "under water");
    }
    (holders, cranker, whale)
}

/// The first round measures: every holder touched, nothing paid, the loss added up; a holding is
/// measured once a round; a step with nothing to do fails (no slot taken). The next round pays
/// every holder the same fraction `b / l_prev` of its uncovered loss, whoever sends the step and in
/// whatever order; a sender cannot change `f`.
#[test]
fn a_measuring_round_then_the_same_fraction_for_everyone() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(&mut w, &launcher, open_terms());
    let (holders, cranker, _whale) = under_water(&mut w, &mint, &[SOL, 2 * SOL, SOL / 2]);
    let hs: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    assert_eq!(cover_state(&w, &mint).round, 0);
    // Round 1: the first step rolls the round (`b` the pot, `l_prev` 0) and touches everyone.
    next_round(&mut w);
    let r1 = round_now(&w);
    let pot = companion_of(&w, &mint).pending_pot;
    let owed_before: Vec<u64> = hs.iter().map(|h| owed(&w, &mint, h)).collect();
    let list: Vec<(Pubkey, bool)> = hs.iter().map(|h| (*h, false)).collect();
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.ok();
    println!(
        "measuring step, 3 holdings: CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    let rounds = cover_rounds(&tx);
    assert_eq!(rounds.len(), 1);
    assert_eq!(
        (
            rounds[0].mint,
            rounds[0].round,
            rounds[0].b,
            rounds[0].l_prev
        ),
        (mint, r1, pot, 0)
    );
    let s = cover_state(&w, &mint);
    let l1: u64 = owed_before.iter().sum();
    assert_eq!((s.round, s.b, s.l_prev, s.l_cur), (r1, pot, 0, l1));
    assert_eq!(
        (s.round_paid, s.measured, s.rounds, s.paid_total),
        (0, 3, 1, 0)
    );
    let payouts = tx.events::<ScriptPayout>();
    assert_eq!(payouts.len(), 3);
    for (h, ev) in hs.iter().zip(&payouts) {
        assert_eq!((ev.owner, ev.lamports, ev.tag), (*h, 0, r1));
        let r = record(&w, &mint, h);
        assert_eq!((r.paid, r.round), (0, r1));
    }
    assert_eq!(tx.event::<ScriptStepped>().bounty, SCRIPT_BOUNTY * 3 / 8);
    assert_eq!(
        companion_of(&w, &mint).pending_pot,
        pot - SCRIPT_BOUNTY * 3 / 8
    );
    // The same holders again this round: nothing to do, the step fails, no slot taken.
    w.env.warp(STEP);
    let last_at = w
        .env
        .read::<Script>(&companion::script_address(&mint))
        .last_at;
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.expect_code(script_code(LossCoverScriptError::NothingToDo));
    assert_eq!(
        w.env
            .read::<Script>(&companion::script_address(&mint))
            .last_at,
        last_at
    );
    assert_eq!(cover_state(&w, &mint).l_cur, l1);
    // Round 2 pays. The pot (the fees so far) covers a part of the loss measured: everyone gets
    // that same part of what they are down, exactly `uncovered * b / l_prev`.
    next_round(&mut w);
    let r2 = round_now(&w);
    let b2 = companion_of(&w, &mint).pending_pot;
    assert!(b2 > 0 && b2 < l1, "b {b2} of L {l1}");
    println!(
        "round 2: b {b2} of L {l1}, the fraction {} bps",
        b2 * 10_000 / l1
    );
    let owed_now: Vec<u64> = hs.iter().map(|h| owed(&w, &mint, h)).collect();
    let before: Vec<u64> = hs.iter().map(|h| w.env.lamports(h)).collect();
    // Two holders by one sender, the third by another, in another order.
    let tx = step(&mut w, &cranker, &mint, &[(hs[1], true), (hs[0], true)]);
    tx.ok();
    println!(
        "paying step, 2 holders: CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    let rounds = cover_rounds(&tx);
    assert_eq!(
        (rounds[0].round, rounds[0].b, rounds[0].l_prev),
        (r2, b2, l1)
    );
    let other = w.wallet_with_sol(SOL);
    w.env.warp(STEP);
    step(&mut w, &other, &mint, &[(hs[2], true)]).ok();
    let s = cover_state(&w, &mint);
    assert_eq!((s.round, s.b, s.l_prev), (r2, b2, l1));
    let mut paid_all = 0;
    for (i, h) in hs.iter().enumerate() {
        let fraction = fraction_of(owed_now[i], b2, l1);
        assert!(fraction > 0 && fraction < owed_now[i]);
        let pay = w.env.lamports(h) - before[i];
        // The fractions of a round add up to its budget; the last holder paid gives way to the
        // bounties the round's steps took from the pot (the companion's bound), never more.
        if i == 2 {
            assert!(
                pay <= fraction && pay + 3 * SCRIPT_BOUNTY >= fraction,
                "holder {i}: {pay} of {fraction}"
            );
        } else {
            assert_eq!(pay, fraction, "holder {i}");
        }
        let r = record(&w, &mint, h);
        assert_eq!((r.paid, r.round), (pay, r2));
        paid_all += pay;
    }
    // The same fraction for all, within rounding and that headroom.
    let f = u128::from(b2) * 1_000_000 / u128::from(l1);
    for (i, h) in hs.iter().enumerate() {
        let pay = w.env.lamports(h) - before[i];
        let got = u128::from(pay) * 1_000_000 / u128::from(owed_now[i]);
        assert!(got <= f + 2 && got + 1_000 >= f, "holder {i}: {got} vs {f}");
    }
    assert_eq!(
        (s.round_paid, s.paid_total, s.measured),
        (paid_all, paid_all, 3)
    );
    assert_eq!(s.l_cur, owed_now.iter().sum::<u64>() - paid_all);
    let t = terms_of(&w, &mint);
    assert_eq!((t.paid_total, t.payouts), (paid_all, 6));
    assert_eq!(t.window_pot, b2);
    let c = companion_of(&w, &mint);
    assert_eq!(
        c.pending_pot,
        b2 - paid_all - SCRIPT_BOUNTY * 2 / 8 - SCRIPT_BOUNTY / 8
    );
    assert_eq!(
        w.env.holding(&w.sol, &companion::creator_address(&mint)),
        c.set_aside().unwrap()
    );
    // Paid this round: a second step naming them does nothing.
    w.env.warp(STEP);
    step(&mut w, &cranker, &mint, &[(hs[0], true)])
        .expect_code(script_code(LossCoverScriptError::NothingToDo));
}

/// The loss grows after a round measured it: the next round's fraction is too high, its budget
/// runs out before the last holders, who are touched instead; the round after includes what they
/// are still owed in `l_prev` and catches up at a smaller fraction. A partial sell keeps its cover
/// pro rata along the way.
#[test]
fn a_budget_that_runs_out_is_caught_up_next_round() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(&mut w, &launcher, open_terms());
    let (holders, cranker, whale) = under_water(&mut w, &mint, &[SOL, SOL, SOL, SOL]);
    let hs: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    // Round 1 measures.
    next_round(&mut w);
    let list: Vec<(Pubkey, bool)> = hs.iter().map(|h| (*h, false)).collect();
    step(&mut w, &cranker, &mint, &list).ok();
    let l1 = cover_state(&w, &mint).l_cur;
    // Before round 2 starts, the price falls further (the whale sells the rest): the loss grows.
    let held = w.env.holding(&mint, &whale.pubkey());
    w.sell(&whale, &mint, held).ok();
    claim(&mut w, &mint);
    let grown: u64 = hs.iter().map(|h| owed(&w, &mint, h)).sum();
    assert!(grown > l1 + l1 / 10, "{grown} vs {l1}");
    // Round 2: the pot about a third of what was measured, so the fraction asks for about two
    // thirds of what everyone is owed now: the budget runs out.
    next_round(&mut w);
    let have = companion_of(&w, &mint).pending_pot;
    let want = l1 / 3;
    if want > have {
        fund_pot(&mut w, &mint, (want - have) * 10_000 / 7_000 + SOL / 1_000);
    }
    let b2 = companion_of(&w, &mint).pending_pot;
    let owed2: Vec<u64> = hs.iter().map(|h| owed(&w, &mint, h)).collect();
    let before: Vec<u64> = hs.iter().map(|h| w.env.lamports(h)).collect();
    let paying: Vec<(Pubkey, bool)> = hs.iter().map(|h| (*h, true)).collect();
    let tx = step(&mut w, &cranker, &mint, &paying);
    tx.ok();
    println!(
        "paying step, 4 holders: CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    let s = cover_state(&w, &mint);
    assert_eq!((s.b, s.l_prev), (b2, l1));
    let pays: Vec<u64> = hs
        .iter()
        .enumerate()
        .map(|(i, h)| w.env.lamports(h) - before[i])
        .collect();
    // The first holders got their full fraction; a later one less, the last nothing (touched).
    assert_eq!(pays[0], fraction_of(owed2[0], b2, l1));
    assert!(pays[3] < fraction_of(owed2[3], b2, l1));
    assert!(pays.iter().sum::<u64>() <= b2);
    assert!(
        s.round_paid >= b2 - SCRIPT_BOUNTY - 1_000,
        "the budget ran out: {} of {b2}",
        s.round_paid
    );
    for (i, h) in hs.iter().enumerate() {
        let r = record(&w, &mint, h);
        assert_eq!((r.paid, r.round), (pays[i], s.round));
    }
    let owed_after: u64 = owed2.iter().sum::<u64>() - pays.iter().sum::<u64>();
    assert_eq!(s.l_cur, owed_after, "what is still owed");
    // Holder 0 sells half: its cover goes with its tokens, pro rata.
    let half = w.env.holding(&mint, &hs[0]) / 2;
    w.sell(&holders[0], &mint, half).ok();
    let r0 = record(&w, &mint, &hs[0]);
    assert_eq!(
        u128::from(r0.paid),
        u128::from(pays[0]) * u128::from(r0.amount) / u128::from(r0.amount + half)
    );
    // Round 3 catches up: `l_prev` is what was owed, the fraction smaller; the pot holds what is
    // left plus new fees.
    next_round(&mut w);
    claim(&mut w, &mint);
    let b3 = companion_of(&w, &mint).pending_pot;
    assert!(b3 > 0);
    let owed3: Vec<u64> = hs.iter().map(|h| owed(&w, &mint, h)).collect();
    let before: Vec<u64> = hs.iter().map(|h| w.env.lamports(h)).collect();
    // The last holders first this time (the keeper's rotation).
    let rotated: Vec<(Pubkey, bool)> = hs.iter().rev().map(|h| (*h, true)).collect();
    let tx = step(&mut w, &cranker, &mint, &rotated);
    tx.ok();
    let s = cover_state(&w, &mint);
    assert_eq!((s.b, s.l_prev), (b3, owed_after));
    assert!(
        s.b * 1_000 / s.l_prev < b2 * 1_000 / l1,
        "a smaller fraction"
    );
    for (i, h) in hs.iter().enumerate() {
        let pay = fraction_of(owed3[i], b3, owed_after);
        assert_eq!(
            w.env.lamports(h) - before[i],
            pay.min(owed3[i]),
            "holder {i}"
        );
    }
}

/// The minimums: a holding below `min_holding`, a loss below `min_loss`, a buy younger than
/// `min_age_secs` and the launcher's own holding are skipped, not measured and not touched; a step
/// of only those fails. A round after a gap measures only, whatever the round before measured.
#[test]
fn the_minimums_the_launcher_and_a_gap() {
    let mut w = world();
    let launcher = w.wallet_with_sol(20 * SOL);
    let min_holding = 1_000 * 1_000_000;
    let terms = CoverTerms {
        cover_bps: 5_000,
        min_holding,
        min_loss: SOL / 100,
        min_age_secs: 600,
        round_secs: ROUND,
        min_pot_lamports: 0,
    };
    let mint = launch_coin(&mut w, &launcher, terms);
    // A whale buys; two holders and the launcher buy high, a tiny holder gets dust from holder 0;
    // ten minutes on a late buyer buys; then the whale sells everything: all of them are under
    // water, the late buyer too young to count.
    let whale = w.wallet_with_sol(40 * SOL);
    w.buy(&whale, &mint, 25 * SOL).ok();
    let holders = [
        holder(&mut w, &mint, 2 * SOL),
        holder(&mut w, &mint, 2 * SOL),
    ];
    let hs: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    w.buy(&launcher, &mint, SOL).ok();
    let tiny = w.wallet_with_sol(SOL);
    w.holdings(&tiny, mint, &[tiny.pubkey()]);
    w.send_tokens(&holders[0], mint, &tiny.pubkey(), min_holding - 1)
        .ok();
    w.env.warp(600);
    let late = holder(&mut w, &mint, SOL);
    let held = w.env.holding(&mint, &whale.pubkey());
    w.sell(&whale, &mint, held).ok();
    let cranker = claim(&mut w, &mint);
    // Half the loss is covered: what each is owed.
    let s = cover_state(&w, &mint);
    assert_eq!(s.cover_bps, 5_000);
    for who in [
        hs[0],
        hs[1],
        launcher.pubkey(),
        tiny.pubkey(),
        late.pubkey(),
    ] {
        assert!(owed(&w, &mint, &who) > 0, "{who} is under water");
    }
    // Round 1: the launcher, the tiny holding and the late buyer are skipped; the two holders
    // measured. A step of only the skipped fails.
    next_round(&mut w);
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        &[
            (launcher.pubkey(), false),
            (tiny.pubkey(), false),
            (late.pubkey(), false),
        ],
    );
    tx.expect_code(script_code(LossCoverScriptError::NothingToDo));
    assert_eq!(cover_state(&w, &mint).round, 0, "no round started");
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        &[
            (launcher.pubkey(), false),
            (hs[0], false),
            (tiny.pubkey(), false),
            (late.pubkey(), false),
            (hs[1], false),
        ],
    );
    tx.ok();
    let payouts = tx.events::<ScriptPayout>();
    assert_eq!(payouts.len(), 2);
    assert_eq!((payouts[0].owner, payouts[1].owner), (hs[0], hs[1]));
    let s = cover_state(&w, &mint);
    assert_eq!(s.measured, 2);
    assert_eq!(s.l_cur, owed(&w, &mint, &hs[0]) + owed(&w, &mint, &hs[1]));
    for skipped in [launcher.pubkey(), tiny.pubkey(), late.pubkey()] {
        assert_eq!(record(&w, &mint, &skipped).round, 0);
    }
    // The late buyer, ten minutes on: measured (in the next round).
    next_round(&mut w);
    w.env.warp(300);
    assert!(w.env.now - record(&w, &mint, &late.pubkey()).basis_at >= 600);
    let tx = step(&mut w, &cranker, &mint, &[(late.pubkey(), true)]);
    tx.ok();
    assert_eq!(tx.events::<ScriptPayout>().len(), 1);
    // A loss below the minimum: a small holder barely under water after a small sell by another.
    let barely = w.wallet_with_sol(2 * SOL);
    w.buy(&barely, &mint, SOL / 100).ok();
    let part = w.env.holding(&mint, &hs[1]) / 50;
    w.sell(&holders[1], &mint, part).ok();
    w.env.warp(600);
    let small = owed(&w, &mint, &barely.pubkey());
    assert!(small > 0 && small < terms.min_loss, "{small}");
    next_round(&mut w);
    step(&mut w, &cranker, &mint, &[(barely.pubkey(), true)])
        .expect_code(script_code(LossCoverScriptError::NothingToDo));
    // The round measured the holders; then two rounds nobody steps: the next round measures only
    // (`l_prev` 0), although the round before measured a loss.
    let list: Vec<(Pubkey, bool)> = hs.iter().map(|h| (*h, true)).collect();
    step(&mut w, &cranker, &mint, &list).ok();
    assert!(cover_state(&w, &mint).l_cur > 0);
    let r = cover_state(&w, &mint).round;
    w.env.warp(3 * R);
    assert_eq!(round_now(&w), r + 3);
    let before: Vec<u64> = hs.iter().map(|h| w.env.lamports(h)).collect();
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.ok();
    let rounds = cover_rounds(&tx);
    assert_eq!((rounds[0].round, rounds[0].l_prev), (r + 3, 0));
    for (i, h) in hs.iter().enumerate() {
        assert_eq!(w.env.lamports(h), before[i], "measured only");
    }
    let s = cover_state(&w, &mint);
    assert_eq!((s.l_prev, s.round_paid), (0, 0));
    assert!(s.l_cur > 0);
    // The next round pays again.
    next_round(&mut w);
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.ok();
    assert!(tx.events::<ScriptPayout>().iter().any(|e| e.lamports > 0));
}

/// A round only starts on a pot of at least `min_pot_lamports`: below it every step fails, the
/// state untouched; once the pot reaches it the round starts (after the gap, measuring only) and
/// the round after pays.
#[test]
fn a_round_needs_the_minimum_pot() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let terms = CoverTerms {
        min_pot_lamports: SOL,
        ..open_terms()
    };
    let mint = launch_coin(&mut w, &launcher, terms);
    assert_eq!(cover_state(&w, &mint).min_pot_lamports, SOL);
    let (holders, cranker, _whale) = under_water(&mut w, &mint, &[SOL, SOL]);
    let hs: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    let list: Vec<(Pubkey, bool)> = hs.iter().map(|h| (*h, true)).collect();
    assert!(companion_of(&w, &mint).pending_pot < SOL);
    next_round(&mut w);
    let r = round_now(&w);
    let last_at = w
        .env
        .read::<Script>(&companion::script_address(&mint))
        .last_at;
    for _ in 0..3 {
        let tx = step(&mut w, &cranker, &mint, &list);
        tx.expect_code(script_code(LossCoverScriptError::NothingToDo));
        assert!(cover_rounds(&tx).is_empty());
        w.env.warp(STEP);
    }
    let s = cover_state(&w, &mint);
    assert_eq!((s.round, s.rounds, s.l_cur, s.measured), (0, 0, 0, 0));
    assert_eq!(
        w.env
            .read::<Script>(&companion::script_address(&mint))
            .last_at,
        last_at
    );
    for h in &hs {
        assert_eq!(record(&w, &mint, h).round, 0);
    }
    // The pot reaches the minimum: the round starts and measures.
    let have = companion_of(&w, &mint).pending_pot;
    fund_pot(&mut w, &mint, (SOL - have) * 10_000 / 7_000 + SOL / 100);
    assert!(companion_of(&w, &mint).pending_pot >= SOL);
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.ok();
    let rounds = cover_rounds(&tx);
    assert_eq!((rounds[0].round, rounds[0].l_prev), (r, 0));
    assert!(rounds[0].b >= SOL);
    assert_eq!(tx.events::<ScriptPayout>().len(), 2);
    assert!(tx.events::<ScriptPayout>().iter().all(|e| e.lamports == 0));
    // The next round pays, the pot still above the minimum.
    next_round(&mut w);
    let before: Vec<u64> = hs.iter().map(|h| w.env.lamports(h)).collect();
    step(&mut w, &cranker, &mint, &list).ok();
    for (i, h) in hs.iter().enumerate() {
        assert!(w.env.lamports(h) > before[i]);
    }
    // The pot spent below the minimum again (the loss is bigger than it): the round after
    // measures nothing until it refills.
    next_round(&mut w);
    if companion_of(&w, &mint).pending_pot < SOL {
        step(&mut w, &cranker, &mint, &list)
            .expect_code(script_code(LossCoverScriptError::NothingToDo));
    }
}

/// After graduation the pool goes on and so does the cover: holders who bought on the curve and
/// are under water in the graduated pool are measured and paid.
#[test]
fn graduation_changes_nothing() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(&mut w, &launcher, open_terms());
    let a = holder(&mut w, &mint, SOL);
    // The curve filled by wallets of 5 SOL each, ten minutes apart so the reference follows the
    // price up (a basis is capped near the reference), then the crossing buy with `graduate`.
    let mut buyers = Vec::new();
    loop {
        let l = w.launch(&mint);
        let p = w.launch_pool(&mint);
        if l.graduation_quote.saturating_sub(p.quote_reserve) <= 2 * SOL {
            break;
        }
        assert!(buyers.len() < 100, "the curve does not fill");
        w.env.warp(600);
        buyers.push(holder(&mut w, &mint, 5 * SOL));
    }
    let last = w.env.funded(1_000 * SOL);
    let amount = w.crossing_buy_amount(&mint, &last.pubkey());
    w.wrap_sol(&last, amount).ok();
    let ixs = [
        token::create_holding(last.pubkey(), mint, last.pubkey()),
        w.launch_swap_ix(&last.pubkey(), &mint, 1, amount, 0),
        w.graduate_ix(&last.pubkey(), &mint),
    ];
    let tx = w.env.send_paid_by(&ixs, &last, &[]);
    tx.ok();
    assert_eq!(
        w.launch(&mint).status,
        bordrless_launch::constants::STATUS_GRADUATED
    );
    let n = buyers.len();
    assert!(n >= 8, "{n} buyers filled the curve");
    // The earliest of them sell everything in the graduated pool, one after the other, until
    // the last buyer on the curve is under water: the price falls below the basis of those who
    // bought after them.
    let latest = buyers[n - 1].pubkey();
    let mut sellers = 0;
    while owed(&w, &mint, &latest) == 0 {
        assert!(sellers < n - 5, "the price does not fall");
        let seller = &buyers[sellers];
        let held = w.env.holding(&mint, &seller.pubkey());
        w.sell(seller, &mint, held).ok();
        sellers += 1;
    }
    let cranker = claim(&mut w, &mint);
    let candidates: Vec<Pubkey> = buyers[sellers..]
        .iter()
        .rev()
        .map(|b| b.pubkey())
        .filter(|b| owed(&w, &mint, b) > 0)
        .take(5)
        .collect();
    assert!(!candidates.is_empty());
    // The basis follows the price up the curve: the first buyer's is below the last one's.
    assert!(record(&w, &mint, &a.pubkey()).entry < record(&w, &mint, &latest).entry);
    next_round(&mut w);
    let list: Vec<(Pubkey, bool)> = candidates.iter().map(|h| (*h, false)).collect();
    step(&mut w, &cranker, &mint, &list).ok();
    assert_eq!(cover_state(&w, &mint).measured as usize, candidates.len());
    next_round(&mut w);
    let list: Vec<(Pubkey, bool)> = candidates.iter().map(|h| (*h, true)).collect();
    let before: Vec<u64> = candidates.iter().map(|h| w.env.lamports(h)).collect();
    let tx = step(&mut w, &cranker, &mint, &list);
    tx.ok();
    for (i, h) in candidates.iter().enumerate() {
        assert!(w.env.lamports(h) > before[i]);
    }
}

/// A step as the keeper sends it: v0 with a compute limit and price, the protocol's table and the
/// four program addresses a companion step adds, plus the hook, the script, the payout authority
/// and the hook authority. The runtime's instruction trace (64) binds before the packet: a touch
/// costs four inner instructions and a payment five on a base of eleven, so a measuring step
/// carries 13 holdings (14 are refused) and a paying step 7 pairs with the protocol table alone (8
/// are one byte over the packet), 10 with a per-coin table of the pairs (11 are refused).
#[test]
fn a_step_fits_a_packet_with_the_protocol_table() {
    use bordrless_program_tests::env::compute_unit_price;
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(&mut w, &launcher, open_terms());
    let sols = [SOL / 4; 16];
    let (holders, cranker, _whale) = under_water(&mut w, &mint, &sols);
    let hs: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    let trace_exceeded = |tx: &Tx| {
        matches!(
            tx.err(),
            solana_transaction::TransactionError::InstructionError(
                _,
                solana_transaction::InstructionError::MaxInstructionTraceLengthExceeded
            )
        )
    };
    let mut table = protocol_lookup_table(&w);
    table.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
        HOOK,
        SCRIPT,
        PAYOUT_AUTHORITY,
        bordrless_hook::hook_authority(&HOOK).0,
        companion::hook_program_data_address(&SCRIPT),
        companion::approval_address(&SCRIPT),
    ]);
    let lut = w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let send = |w: &mut World, list: &[(Pubkey, bool)]| {
        let ix = step_ix(w, cranker.pubkey(), &mint, list);
        w.env.send_v0(
            &[
                compute_unit_limit(1_400_000),
                compute_unit_price(20_000),
                ix,
            ],
            &cranker,
            &[],
            std::slice::from_ref(&lut),
        )
    };
    // Round 1: a first small step (a coin's first step also makes the creator's token holding,
    // two more instructions: that step carries two holdings fewer), then 14 holdings are over
    // the trace and 13 fit, then the one left.
    next_round(&mut w);
    let list: Vec<(Pubkey, bool)> = hs[14..].iter().map(|h| (*h, false)).collect();
    send(&mut w, &list).ok();
    w.env.warp(STEP);
    let list: Vec<(Pubkey, bool)> = hs[..14].iter().map(|h| (*h, false)).collect();
    let tx = send(&mut w, &list);
    assert!(trace_exceeded(&tx), "{:?}", tx.err());
    let list: Vec<(Pubkey, bool)> = hs[..13].iter().map(|h| (*h, false)).collect();
    let tx = send(&mut w, &list);
    tx.ok();
    println!(
        "measuring step, 13 holdings, v0 with the table: {} bytes, CU {}, trace {}, height {}",
        tx.size,
        tx.cu(),
        tx.trace_len(),
        tx.max_height()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.trace_len() <= 64);
    assert_eq!(tx.events::<ScriptPayout>().len(), 13);
    w.env.warp(STEP);
    let list: Vec<(Pubkey, bool)> = hs[13..14].iter().map(|h| (*h, false)).collect();
    send(&mut w, &list).ok();
    assert_eq!(cover_state(&w, &mint).measured, 16);
    // Round 2 pays: 7 pairs a step, then the other 9 (10 fit; 11 are over the trace, round 3).
    next_round(&mut w);
    let have = companion_of(&w, &mint).pending_pot;
    let l1 = cover_state(&w, &mint).l_cur;
    if have < l1 / 4 {
        fund_pot(&mut w, &mint, (l1 / 4 - have) * 10_000 / 7_000 + SOL / 100);
    }
    let list: Vec<(Pubkey, bool)> = hs[..7].iter().map(|h| (*h, true)).collect();
    let tx = send(&mut w, &list);
    tx.ok();
    println!(
        "paying step, 7 pairs, v0 with the table: {} bytes, CU {}, trace {}, height {}",
        tx.size,
        tx.cu(),
        tx.trace_len(),
        tx.max_height()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.trace_len() <= 64);
    assert!(tx.max_height() <= 5);
    let paid = tx.events::<ScriptPayout>();
    assert_eq!(paid.len(), 7);
    assert!(paid.iter().all(|e| e.lamports > 0));
    // The bounty grows with the entries: seven of the eight that earn it whole.
    assert_eq!(tx.event::<ScriptStepped>().bounty, SCRIPT_BOUNTY * 7 / 8);
    // One pair more is over the packet with the protocol table alone (not due: nothing paid).
    let list: Vec<(Pubkey, bool)> = hs[7..15].iter().map(|h| (*h, true)).collect();
    let tx = send(&mut w, &list);
    println!("paying step, 8 pairs: {} bytes", tx.size);
    assert!(tx.size > 1_232, "{} bytes", tx.size);
    // The pairs' holdings and owners in a per-coin table, as the keeper keeps one for a larger
    // coin: the other 9 pairs fit the packet, and 10 the trace (round 3: 11 are refused).
    let mut pairs = Vec::new();
    for h in &hs {
        pairs.push(token::holding_address(&mint, h));
        pairs.push(*h);
    }
    let coin = w.env.put_lookup_table(Pubkey::new_unique(), &pairs);
    let both = [lut.clone(), coin];
    let send_both = |w: &mut World, list: &[(Pubkey, bool)]| {
        let ix = step_ix(w, cranker.pubkey(), &mint, list);
        w.env.send_v0(
            &[
                compute_unit_limit(1_400_000),
                compute_unit_price(20_000),
                ix,
            ],
            &cranker,
            &[],
            &both,
        )
    };
    w.env.warp(STEP);
    let list: Vec<(Pubkey, bool)> = hs[7..].iter().map(|h| (*h, true)).collect();
    let tx = send_both(&mut w, &list);
    tx.ok();
    println!(
        "paying step, 9 pairs, with a per-coin table: {} bytes, CU {}, trace {}",
        tx.size,
        tx.cu(),
        tx.trace_len()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert_eq!(tx.events::<ScriptPayout>().len(), 9);
    // Round 3 with a budget again (a holder the budget leaves out is touched, not paid).
    next_round(&mut w);
    let have = companion_of(&w, &mint).pending_pot;
    let l2 = cover_state(&w, &mint).l_cur;
    if have < l2 / 4 {
        fund_pot(&mut w, &mint, (l2 / 4 - have) * 10_000 / 7_000 + SOL / 100);
    }
    let list: Vec<(Pubkey, bool)> = hs[..11].iter().map(|h| (*h, true)).collect();
    let tx = send_both(&mut w, &list);
    assert!(trace_exceeded(&tx), "{:?}", tx.err());
    let list: Vec<(Pubkey, bool)> = hs[..10].iter().map(|h| (*h, true)).collect();
    let tx = send_both(&mut w, &list);
    tx.ok();
    println!(
        "paying step, 10 pairs, with a per-coin table: {} bytes, CU {}, trace {}",
        tx.size,
        tx.cu(),
        tx.trace_len()
    );
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert_eq!(tx.events::<ScriptPayout>().len(), 10);
}

/// What the keeper reads: the script's state and the terms decode where the design says; a step
/// without the payout accounts can't pay (the companion's `MissingAccount`).
#[test]
fn the_state_and_the_terms_are_where_the_keeper_reads_them() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let terms = CoverTerms {
        cover_bps: 8_000,
        min_holding: 7,
        min_loss: 9,
        min_age_secs: 11,
        round_secs: 600,
        min_pot_lamports: 13,
    };
    let mint = launch_coin(&mut w, &launcher, terms);
    let state = w
        .env
        .account(&companion::script_state_address(&mint))
        .unwrap();
    assert_eq!((state.owner, state.data.len()), (SCRIPT, 128));
    let s = cover_state(&w, &mint);
    assert_eq!(s.terms(), terms);
    assert_eq!((s.version, s.mint, s.round, s.rounds), (1, mint, 0, 0));
    assert_eq!(&state.data[67..75], &13u64.to_le_bytes());
    assert_eq!(&state.data[63..67], &600u32.to_le_bytes());
    let t = terms_of(&w, &mint);
    assert_eq!((t.window_secs, t.step_bps), (600, 10_000));
    // Out-of-bounds terms fail the setup.
    for bad in [
        CoverTerms {
            cover_bps: 999,
            ..terms
        },
        CoverTerms {
            round_secs: 59,
            ..terms
        },
        CoverTerms {
            min_pot_lamports: 100 * SOL + 1,
            ..terms
        },
    ] {
        let mint = Keypair::new();
        let args = CreateScriptArgs {
            split: Split {
                buyback_bps: 0,
                holders_bps: 0,
                beneficiary_bps: 3_000,
            },
            pot_bps: 7_000,
            program: SCRIPT,
            interval: STEP,
            state_len: STATE_LEN,
            init_data: borsh(&bad),
        };
        let payout = PayoutTermsArgs {
            step_bps: 10_000,
            holding_bps: 10_000,
            window_bps: 10_000,
            window_secs: 300,
        };
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            companion::create_script_v2(launcher.pubkey(), mint.pubkey(), args, payout, vec![]),
        ];
        let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
        tx.expect_code(script_code(LossCoverScriptError::BadTerms));
    }
    // A step without the hook's accounts: the script finds no terms.
    let (holders, cranker, _whale) = under_water(&mut w, &mint, &[SOL]);
    next_round(&mut w);
    let ix = companion::script_step(
        cranker.pubkey(),
        &keys(&w, &mint),
        SCRIPT,
        vec![AccountMeta::new(
            token::holding_address(&mint, &holders[0].pubkey()),
            false,
        )],
    );
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(script_code(LossCoverScriptError::NoTerms));
    let _ = CompanionError::MissingAccount;
}
