//! Phase 3a audit, round 1: economic manipulation and the strategy game (independent auditor).
//!
//! Findings (`m*`) and "checked, fine" (`f*`) proofs of concept against `pay_strategy`,
//! `plan_period`, `create_strategy_game`, the lottery tickets and Studio's pro-rata starter. Report:
//! `bordrless-games-work/log-3a-audit-manip-r1.md`.
//!
//! Scripted strategy behaviour comes only from the test-only tester's config account (put here),
//! as in `companion_strategy.rs`, whose harness this file copies (a test file can't import another).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorDeserialize, InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs, StrategyArgs};
use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, Split, StrategyTerms};
use bordrless_core::policy;
use bordrless_game::{round_of, Slots};
use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_token::client::{self as token, Hook};
use lottery_hook::client as lottery;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use strategy_tester as st;

const ROUND: u32 = 3_600;
const R: i64 = ROUND as i64;
const CREATOR_FEE: u16 = 200;
const BOUNTY_BPS: u16 = 50;
const SPLIT: Split = Split {
    buyback_bps: 3_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const POT_BPS: u16 = 7_000;
const MIN_POT: u64 = 100_000_000;
const WINDOW: u32 = 600;
const HOOK: Pubkey = lottery_hook::ID;
const STRATEGY: Pubkey = st::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const SYSTEM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

// ------------------------------------------------------------------------------------- harness

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: BOUNTY_BPS,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn game_args() -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Strategy,
        hook: HOOK,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs: ROUND,
        min_pot: MIN_POT,
        prize_bps: 0,
        claim_window_secs: WINDOW,
        max_attempts: 0,
    }
}

fn strategy_args() -> StrategyArgs {
    StrategyArgs {
        strategy: STRATEGY,
        budget_bps: MAX_STRATEGY_BUDGET_BPS,
        max_share_bps: MAX_STRATEGY_SHARE_BPS,
        max_per_tx: MAX_STRATEGY_PER_TX,
        plan_cu_max: MAX_PLAN_CU,
        entitle_cu_max: MAX_ENTITLE_CU,
        min_weight: 1,
    }
}

fn config_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config", mint.as_ref()], &STRATEGY).0
}

fn pda_seeds(literal: &[u8]) -> ExtraAccount {
    ExtraAccount {
        writable: false,
        source: AccountSource::Pda {
            program: STRATEGY,
            seeds: vec![Seed::Literal(literal.to_vec()), Seed::Account(0)],
        },
    }
}

fn put_owned(w: &mut World, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
    let lamports = w.env.rent(data.len());
    w.env.put(
        key,
        Account {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    );
}

fn prepare_strategy(w: &mut World, mint: &Pubkey) {
    put_owned(
        w,
        config_address(mint),
        STRATEGY,
        st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_PRO_RATA, 0),
    );
    put_owned(
        w,
        bordrless_strategy::registry_address(&STRATEGY, mint).0,
        STRATEGY,
        HookAccountList::new(vec![pda_seeds(b"config")]).encode(),
    );
}

fn world(authority: Option<Pubkey>) -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(STRATEGY, &program_bytes("strategy_tester"))
        .expect("load the tester");
    w.env.set_upgrade_authority(STRATEGY, authority);
    w
}

fn hook_config(w: &mut World, creator: &Keypair) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(HOOK),
            custom_hook_flags: lottery_hook::FLAGS,
            label: "Strategy".to_string(),
        },
    );
    tx.ok();
    config
}

fn launch_ix(w: &World, launcher: &Pubkey, mint: &Pubkey, config: &Pubkey) -> Instruction {
    let c = w.launch_config(config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, mint));
    let mut args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
    args.name = "Strategy".to_string();
    let inner = launch::create_launch_with(
        companion::creator_address(mint),
        *mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        Some(*config),
        custom.as_ref(),
    );
    companion::launch(*launcher, *mint, &inner, args)
}

fn extras(mint: &Pubkey) -> Vec<Pubkey> {
    vec![config_address(mint)]
}

struct Strat {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
}

impl Strat {
    fn new() -> Self {
        Self::with_world(world(Some(STUDIO_KEY)))
    }

    /// A strategy coin in `w` (statuses or authorities set up before), launched and 31 s on.
    fn with_world(mut w: World) -> Self {
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(&mut w, &mint);
        let ixs = vec![
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, ROUND),
            companion::create_strategy_game(
                launcher.pubkey(),
                mint,
                game_args(),
                strategy_args(),
                vec![],
                false,
                &extras(&mint),
            ),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
        let config = hook_config(&mut w, &launcher);
        let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config);
        w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self { w, mint, cranker }
    }

    fn round(&self) -> u32 {
        round_of(self.w.env.now, ROUND)
    }

    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * R + secs;
        assert!(t >= self.w.env.now);
        self.w.env.warp(t - self.w.env.now);
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn balance(&self, owner: &Pubkey) -> u64 {
        self.w.env.holding(&self.mint, owner)
    }

    fn slots(&self, owner: &Pubkey) -> Slots {
        Slots::decode(&self.w.env.hook_data(&self.mint, owner))
    }

    fn weight(&self, owner: &Pubkey, round: u32) -> u64 {
        self.slots(owner).range_in(round).map_or(0, |r| r.weight)
    }

    fn buyer(&mut self, sol: u64) -> Keypair {
        let t = self.w.wallet_with_sol(sol + SOL);
        self.w.buy(&t, &self.mint, sol).ok();
        t
    }

    fn volume(&mut self, wallets: usize, sol: u64) {
        for _ in 0..wallets {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            let held = self.balance(&t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
        }
    }

    fn enter(&mut self, holders: &[&Keypair]) {
        for h in holders {
            let ix = lottery::enter(self.mint, h.pubkey());
            self.send(ix).ok();
        }
    }

    fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
        let next = self.round() + 1;
        self.warp_into(next, 5);
        self.enter(holders);
        next
    }

    fn claim_fees_ix(&self) -> Instruction {
        companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK)
    }

    fn fund_pot(&mut self) {
        self.volume(1, 20 * SOL);
        let ix = self.claim_fees_ix();
        self.send(ix).ok();
        assert!(self.companion().pending_pot >= MIN_POT);
    }

    fn set_config(&mut self, plan: (u8, u64), entitle: (u8, u64)) {
        let mint = self.mint;
        put_owned(
            &mut self.w,
            config_address(&mint),
            STRATEGY,
            st::config_bytes(plan.0, plan.1, entitle.0, entitle.1),
        );
    }

    fn plan_ix(&self, period: u32) -> Instruction {
        companion::plan_period(
            self.cranker.pubkey(),
            self.mint,
            HOOK,
            STRATEGY,
            self.w.launch(&self.mint).pool,
            &extras(&self.mint),
            period,
        )
    }

    fn plan(&mut self, period: u32) -> Tx {
        let ix = self.plan_ix(period);
        self.send(ix)
    }

    fn pay_ix(&self, period: u32, owners: &[Pubkey]) -> Instruction {
        companion::pay_strategy(
            self.cranker.pubkey(),
            self.mint,
            HOOK,
            STRATEGY,
            &extras(&self.mint),
            period,
            owners,
        )
    }

    fn pay(&mut self, period: u32, owners: &[Pubkey]) -> Tx {
        let ix = self.pay_ix(period, owners);
        let cranker = self.cranker.insecure_clone();
        self.w
            .env
            .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[])
    }

    /// `amount` tokens from `from` to `to`'s holding (created first), through the lottery hook.
    fn transfer(&mut self, from: &Keypair, to: &Pubkey, amount: u64) -> Tx {
        let custom = self.w.custom_hook_accounts(&HOOK, &self.mint);
        let ixs = [
            token::create_holding(from.pubkey(), self.mint, *to),
            token::transfer_with(
                from.pubkey(),
                token::holding_address(&self.mint, &from.pubkey()),
                token::holding_address(&self.mint, to),
                self.mint,
                Some(Hook::of(HOOK)),
                custom.extras,
                amount,
            ),
        ];
        self.w.env.send_paid_by(&ixs, from, &[])
    }

    fn retire(&mut self) -> Tx {
        let ix = companion::retire_game(self.cranker.pubkey(), self.mint, HOOK);
        self.send(ix)
    }
}

fn reasons(tx: &Tx) -> Vec<(Pubkey, CandidateReason)> {
    tx.events::<CandidateRejected>()
        .into_iter()
        .map(|e| (e.owner, e.reason))
        .collect()
}

fn bps(amount: u64, bps: u64) -> u64 {
    (u128::from(amount) * u128::from(bps) / 10_000) as u64
}

// =============================================================================== findings

/// M-1. `plan_period` and `pay_strategy` are both permissionless and compose in one transaction:
/// whoever plans can pay their own candidates in the same atomic transaction, before any keeper
/// can act. For any first-come strategy (a flat amount per holder, "the first N holders over a
/// threshold", Studio's listed starter), four dust-holding sybils of the planner take the whole
/// budget (4 x the 25% cap) and the big honest holders are refused (`OverBound`) for the period.
#[test]
fn m1_plan_and_pay_in_one_transaction_captures_a_first_come_budget() {
    let mut s = Strat::new();
    let honest_a = s.buyer(5 * SOL);
    let honest_b = s.buyer(3 * SOL);
    // The planner's sybils: dust each (weight >= min_weight is all a flat strategy asks).
    let sybils: Vec<Keypair> = (0..4).map(|_| s.buyer(SOL / 1_000)).collect();
    let mut all: Vec<&Keypair> = vec![&honest_a, &honest_b];
    all.extend(sybils.iter());
    let p = s.start_round(&all);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    let pot = s.companion().pending_pot;
    let budget = bps(pot, u64::from(MAX_STRATEGY_BUDGET_BPS));
    let cap = bps(budget, u64::from(MAX_STRATEGY_SHARE_BPS));
    // A flat-per-holder strategy (the "first N" starter): every candidate gets the cap.
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, cap));
    for k in &sybils {
        assert!(s.weight(&k.pubkey(), p) > 0);
        assert!(s.weight(&k.pubkey(), p) * 100 < s.weight(&honest_a.pubkey(), p));
    }
    // One transaction: plan, then pay the planner's own four wallets.
    let owners: Vec<Pubkey> = sybils.iter().map(|k| k.pubkey()).collect();
    let cranker = s.cranker.insecure_clone();
    let ixs = [compute_unit_limit(1_400_000), s.plan_ix(p), s.pay_ix(p, &owners)];
    let mut table = protocol_lookup_table(&s.w);
    table.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    let lut = s.w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let tx = s.w.env.send_v0(&ixs, &cranker, &[], std::slice::from_ref(&lut));
    tx.ok();
    let bundle_size = tx.size;
    assert!(bundle_size <= 1_232, "{bundle_size}");
    let planned: PeriodPlanned = tx.event();
    assert_eq!(planned.budget, budget);
    let paid: StrategyPaid = tx.event();
    assert_eq!(paid.payments.len(), 4);
    assert_eq!(paid.epoch_paid, budget - budget % 4);
    // The honest holders, 100x the weight each, get nothing this period.
    let tx = s.pay(p, &[honest_a.pubkey(), honest_b.pubkey()]);
    tx.ok();
    let r = reasons(&tx);
    assert_eq!(r.len(), 2);
    assert!(r
        .iter()
        .all(|(_, why)| *why == CandidateReason::Answer(AnswerFault::OverBound)));
    println!(
        "M-1: plan+pay in one tx ({} bytes with the protocol's table): 4 dust sybils took {} of a {} budget",
        bundle_size, paid.epoch_paid, budget
    );
}

/// M-2. A strategy is not limited to "one candidate at a time": `sol_get_processed_sibling_
/// instruction` (active, agave 4.3 `solana-syscalls/src/lib.rs:466, 2008-2080`) returns every
/// earlier CPI the companion made in the same instruction at the strategy's stack height. In a
/// `pay_strategy` of [A, B], `entitle(B)` is preceded at height 2 by `entitle(A)` (its data: A's
/// owner, balance, weight, since, paid) and by the system program's `create_account` of A's
/// receipt, whose first account is the cranker (a signer). So `entitle` can depend on who else is
/// in the transaction and on who cranks it. This test proves the trace the syscall reads.
#[test]
fn m2_entitle_can_read_earlier_candidates_and_the_cranker_as_siblings() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let b = s.buyer(SOL);
    let p = s.start_round(&[&a, &b]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.plan(p).ok();
    let tx = s.pay(p, &[a.pubkey(), b.pubkey()]);
    let meta = tx.ok();
    // The pay instruction is the second (after the compute budget).
    let inner = &meta.inner_instructions[1];
    let height2: Vec<(Pubkey, Vec<Pubkey>, Vec<u8>)> = inner
        .iter()
        .filter(|i| i.stack_height == 2)
        .map(|i| {
            let ix = &i.instruction;
            (
                tx.keys[usize::from(ix.program_id_index)],
                ix.accounts.iter().map(|k| tx.keys[usize::from(*k)]).collect(),
                ix.data.clone(),
            )
        })
        .collect();
    let entitles: Vec<usize> = height2
        .iter()
        .enumerate()
        .filter(|(_, (p, _, d))| *p == STRATEGY && d[..8] == bordrless_strategy::ENTITLE)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(entitles.len(), 2);
    let (first, second) = (entitles[0], entitles[1]);
    // Fixed (round 1): every question comes before any receipt, so the siblings of entitle(B)
    // are entitle(A) (another candidate's arguments: Studio's checks refuse the sibling syscall)
    // and never an instruction naming the sender.
    let args_a = bordrless_strategy::EntitleArgs::try_from_slice(&height2[first].2[8..]).unwrap();
    assert_eq!(args_a.owner, a.pubkey());
    assert!(height2[..second]
        .iter()
        .all(|(_, accts, _)| !accts.contains(&s.cranker.pubkey())));
    assert!(!height2[..second].iter().any(|(p, _, _)| *p == SYSTEM));
    // The receipts come after: A's, then B's, each paid by the sender.
    let creates: Vec<&(Pubkey, Vec<Pubkey>, Vec<u8>)> = height2[second + 1..]
        .iter()
        .filter(|(p, accts, _)| *p == SYSTEM && accts.len() == 2 && accts[0] == s.cranker.pubkey())
        .collect();
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0].1[1], companion::receipt_address(&s.mint, p, &a.pubkey()));
    assert_eq!(creates[1].1[1], companion::receipt_address(&s.mint, p, &b.pubkey()));
}

/// M-3. `since` (passed to `entitle`, the basis of the §4.9 "loyalty" starter) is set at the
/// first receive and moved only by sends: a wallet that received dust long ago and tops up just
/// before a period gets the full weight AND the old `since`. A loyalty multiplier on `now - since`
/// is bought by keeping a dust wallet aged, never by holding.
#[test]
fn m3_since_is_old_for_a_dust_wallet_topped_up_just_before_the_period() {
    let mut s = Strat::new();
    let aged = s.buyer(SOL / 1_000);
    let since_dust = s.slots(&aged.pubkey()).since;
    assert!(since_dust > 0);
    // 30 days later, a minute before a period starts, the aged wallet buys 5 SOL.
    s.w.env.warp(30 * 86_400);
    let p = s.round() + 1;
    s.warp_into(p, -60);
    s.w.env.fund(aged.pubkey(), 8 * SOL);
    s.w.wrap_sol(&aged, 6 * SOL).ok();
    s.w.buy(&aged, &s.mint.clone(), 5 * SOL).ok();
    // An honest wallet that bought 5 SOL at the same moment.
    let fresh = s.buyer(5 * SOL);
    s.warp_into(p, 5);
    s.enter(&[&aged, &fresh]);
    let (wa, wf) = (s.weight(&aged.pubkey(), p), s.weight(&fresh.pubkey(), p));
    // Both hold their whole balance as weight (held through the period)...
    assert_eq!(wa, s.balance(&aged.pubkey()));
    assert_eq!(wf, s.balance(&fresh.pubkey()));
    // ...but `since` (what `entitle` gets) is 30 days old for one and a minute for the other.
    let since_aged = s.slots(&aged.pubkey()).since;
    let since_fresh = s.slots(&fresh.pubkey()).since;
    assert_eq!(since_aged, since_dust);
    assert!(since_fresh - since_aged >= 30 * 86_400);
    println!("M-3: aged since {since_aged}, fresh since {since_fresh}, weights {wa} / {wf}");
}

/// M-4 (griefing). An answer over `budget_max` closes the period with nothing (fail closed, never
/// clamped), and anyone chooses when to plan. A strategy with a budget of its own (not clamped to
/// `budget_max`) is griefed by a plan sent before the keeper's fee claim: the period is lost.
#[test]
fn m4_a_plan_sent_before_the_fee_claim_voids_an_unclamped_budget() {
    let run = |claim_first: bool| -> Tx {
        let mut s = Strat::new();
        let a = s.buyer(5 * SOL);
        let p = s.start_round(&[&a]);
        s.fund_pot();
        // More trading: fees the keeper would claim with the plan.
        s.volume(1, 20 * SOL);
        s.warp_into(p + 1, 10);
        let pot = s.companion().pending_pot;
        // The strategy plans a fixed budget just above half the unclaimed-fee-less pot.
        s.set_config((st::MODE_FLAT, pot / 2 + 1), (st::MODE_PRO_RATA, 0));
        let cranker = s.cranker.insecure_clone();
        let mut ixs = vec![];
        if claim_first {
            ixs.push(s.claim_fees_ix());
        }
        ixs.push(s.plan_ix(p));
        let tx = s.w.env.send_paid_by(&ixs, &cranker, &[]);
        tx.ok();
        if claim_first {
            assert_eq!(s.game().next_round, p + 1);
        } else {
            // Fixed (round 1): a refused answer uses up nothing; the keeper's claim and plan
            // still plan the period.
            assert!(s.game().next_round <= p, "a refused plan leaves the period open");
            let ixs = vec![s.claim_fees_ix(), s.plan_ix(p)];
            let again = s.w.env.send_paid_by(&ixs, &cranker, &[]);
            again.ok();
            assert!(!again.events::<PeriodPlanned>().is_empty());
            assert_eq!(s.game().next_round, p + 1);
        }
        tx
    };
    // The keeper (claim, then plan): planned.
    let tx = run(true);
    assert!(!tx.events::<PeriodPlanned>().is_empty());
    // A griefer plans first: rejected, and the period is still the keeper's to plan.
    let tx = run(false);
    assert_eq!(tx.event::<PeriodRejected>().reason, AnswerFault::OverBound);
}

/// M-5. Any payment moves `settled_at`, so a strategy that pays one lamport a period (to a wallet
/// of its author's) keeps the pot from ever being retired: "no pot is locked for ever" (§4.3)
/// does not hold. The pot sits at its cap; nobody else is paid.
#[test]
fn m5_dust_payments_keep_the_pot_from_retiring() {
    let mut s = Strat::new();
    let a = s.buyer(SOL);
    let launched = s.w.env.now;
    let p = s.start_round(&[&a]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    // A budget of 1,000 lamports, one lamport per holder.
    s.set_config((st::MODE_FLAT, 1_000), (st::MODE_FLAT, 1));
    s.plan(p).ok();
    s.pay(p, &[a.pubkey()]).ok();
    // 59 days on: one more dust period.
    s.w.env.warp(59 * 86_400);
    let q = s.start_round(&[&a]);
    s.warp_into(q + 1, 10);
    s.plan(q).ok();
    let tx = s.pay(q, &[a.pubkey()]);
    tx.ok();
    assert_eq!(tx.event::<StrategyPaid>().payments[0].amount, 1);
    let pot = s.companion().pending_pot;
    // Fixed (round 1): dust (below `STRATEGY_ACTIVE_BPS` of the pot in a period) is no activity,
    // so 61 days after the launch the pot retires.
    assert_eq!(s.game().settled_at, 0);
    s.w.env.warp(launched + 61 * 86_400 - s.w.env.now);
    s.warp_into(s.round(), (R - i64::from(WINDOW)) + 5);
    assert!(pot > MIN_POT);
    s.retire().ok();
    assert_eq!(s.companion().pending_pot, 0);
}

/// M-6. A strategy's class is checked once, at `create_strategy_game`. A Bordrless-managed strategy
/// whose authority is later handed to an outside key (Studio's hot key doing a handover, by mistake
/// or compromise) keeps planning and paying while author-upgradeable; a new game with it is refused.
#[test]
fn m6_the_strategys_class_is_not_rechecked_after_creation() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let p = s.start_round(&[&a]);
    s.fund_pot();
    let outsider = Keypair::new().pubkey();
    s.w.env.set_upgrade_authority(STRATEGY, Some(outsider));
    s.warp_into(p + 1, 10);
    // Fixed (round 1): the class is read again before every question; handed to an outsider, the
    // strategy is asked nothing.
    refused(&s.plan(p), CompanionError::StrategyNotAccepted);
    // Back under Studio's key, it plans; handed away again mid-period, its payments stop.
    s.w.env.set_upgrade_authority(STRATEGY, Some(STUDIO_KEY));
    s.w.env.svm.expire_blockhash();
    s.plan(p).ok();
    s.w.env.set_upgrade_authority(STRATEGY, Some(outsider));
    refused(&s.pay(p, &[a.pubkey()]), CompanionError::StrategyNotAccepted);
    // The same program is refused for a new game now.
    let mut w2 = std::mem::replace(&mut s.w, World::new());
    let launcher = w2.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    prepare_strategy(&mut w2, &mint);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        companion::create_strategy_game(
            launcher.pubkey(),
            mint,
            game_args(),
            strategy_args(),
            vec![],
            false,
            &extras(&mint),
        ),
    ];
    refused(
        &w2.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::StrategyNotAccepted,
    );
}

/// M-7. `set_hook_status` (v1)'s new refusal of an audit of non-fixed code applies only when the
/// caller passes the ProgramData: without it, an author-upgradeable strategy is recorded audited
/// (no hash), `vet_strategy` takes any status as vetting (class `STATUS`), and with the ticket hook
/// audited the strategy game is uncapped while its author can replace the code at any time.
#[test]
fn m7_a_v1_audit_without_programdata_admits_an_author_upgradeable_strategy_uncapped() {
    let author = Keypair::new();
    let mut w = world(Some(author.pubkey()));
    let deployer = w.env.deployer.insecure_clone();
    let audit = |program: Pubkey| {
        companion::set_hook_status(
            deployer.pubkey(),
            program,
            HookStatusArgs {
                audited: true,
                pot_cap: 0,
                blocked: false,
            },
        )
    };
    // With its ProgramData passed: refused.
    let mut ix = audit(STRATEGY);
    ix.accounts.push(AccountMeta::new_readonly(
        companion::hook_program_data_address(&STRATEGY),
        false,
    ));
    refused(
        &w.env.send_paid_by(&[ix], &deployer, &[]),
        CompanionError::AuditNeedsFixedCode,
    );
    // Without: recorded.
    w.env.send_paid_by(&[audit(STRATEGY)], &deployer, &[]).ok();
    w.env.send_paid_by(&[audit(HOOK)], &deployer, &[]).ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    prepare_strategy(&mut w, &mint);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        companion::create_strategy_game(
            launcher.pubkey(),
            mint,
            game_args(),
            strategy_args(),
            vec![],
            false,
            &extras(&mint),
        ),
    ];
    // Fixed (round 1): a status never lets an author-upgradeable strategy in.
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::StrategyNotAccepted,
    );
    // And a v1 audit (no hash) of a Bordrless-managed strategy lifts no cap: only a v2 audit does.
    let mut w = world(Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let audit = |program: Pubkey| {
        companion::set_hook_status(
            deployer.pubkey(),
            program,
            HookStatusArgs {
                audited: true,
                pot_cap: 0,
                blocked: false,
            },
        )
    };
    w.env.send_paid_by(&[audit(STRATEGY)], &deployer, &[]).ok();
    w.env.send_paid_by(&[audit(HOOK)], &deployer, &[]).ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    prepare_strategy(&mut w, &mint);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        companion::create_strategy_game(
            launcher.pubkey(),
            mint,
            game_args(),
            strategy_args(),
            vec![],
            false,
            &extras(&mint),
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
    tx.ok();
    let set: StrategySet = tx.event();
    assert_eq!(
        (set.class, set.audited, set.pot_cap),
        (strategy_class::STATUS, false, DEFAULT_POT_CAP)
    );
}

/// M-8. Studio's pro-rata starter (`fixtures/strategies/pro_rata`, "what a Studio strategy is") has
/// a `prepare` that anyone may call for any mint key, no mint signature (unlike `lottery_hook`'s
/// `prepare`, which the mint signs). Whoever prepares first fixes the mint's budget share for ever:
/// an attacker who learns a mint address before its setup lands (a vanity mint, a setup sent in
/// two transactions, a leaked bundle) sets 1 bp, and the launcher either fails (its own prepare is
/// refused) or launches with the attacker's settings.
#[test]
fn m8_the_starters_prepare_is_anyones_for_any_mint() {
    const STARTER: Pubkey = strategy_pro_rata::ID;
    let mut w = World::new();
    w.env
        .svm
        .add_program(STARTER, &program_bytes("strategy_pro_rata"))
        .expect("load the starter");
    w.env.set_upgrade_authority(STARTER, None);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let state =
        Pubkey::find_program_address(&[strategy_pro_rata::STATE_SEED, mint.as_ref()], &STARTER).0;
    let prepare = |payer: Pubkey, bps: u16| Instruction {
        program_id: STARTER,
        accounts: strategy_pro_rata::accounts::Prepare {
            payer,
            mint,
            state,
            registry: bordrless_strategy::registry_address(&STARTER, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: strategy_pro_rata::instruction::Prepare {
            budget_bps_of_max: bps,
        }
        .data(),
    };
    // Fixed (round 1): the mint signs `prepare`. The attacker, without the mint's key (the mint
    // left unsigned in its instruction): refused.
    let attacker = w.wallet_with_sol(SOL);
    let mut ix = prepare(attacker.pubkey(), 1);
    for m in ix.accounts.iter_mut() {
        if m.pubkey == mint {
            m.is_signer = false;
        }
    }
    w.env.send_paid_by(&[ix], &attacker, &[]).expect_fail();
    assert!(w.env.account(&state).is_none());
    // The launcher's setup with its own prepare: made, on its own settings.
    let launcher = w.wallet_with_sol(5 * SOL);
    let s = StrategyArgs {
        strategy: STARTER,
        ..strategy_args()
    };
    let setup = |with_prepare: bool| {
        let mut ixs = vec![
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, ROUND),
        ];
        if with_prepare {
            ixs.push(prepare(launcher.pubkey(), 10_000));
        }
        ixs.push(companion::create_strategy_game(
            launcher.pubkey(),
            mint,
            game_args(),
            s,
            vec![],
            false,
            &[state],
        ));
        ixs
    };
    w.env
        .send_paid_by(&setup(true), &launcher, &[&mint_kp])
        .ok();
    let t: StrategyTerms = w.env.read(&companion::strategy_terms_address(&mint));
    assert_eq!(t.extras(), &[state][..]);
    let data = w.env.account(&state).unwrap().data;
    assert_eq!(u16::from_le_bytes([data[40], data[41]]), 10_000);
    let _ = setup(false);
}

// =============================================================================== checked, fine

/// Moving a period's tokens between one's own wallets after the period gains nothing: the sender's
/// slots are cut (or cleared) and the receiver had no weight for it.
#[test]
fn f1_moving_tokens_between_own_wallets_after_the_period_gains_nothing() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let b = s.buyer(2 * SOL);
    let p = s.start_round(&[&a, &b]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.plan(p).ok();
    // A moves everything to a fresh wallet of its own before being paid.
    let a2 = s.w.wallet_with_sol(SOL);
    let all = s.balance(&a.pubkey());
    s.transfer(&a, &a2.pubkey(), all).ok();
    let tx = s.pay(p, &[a.pubkey(), a2.pubkey()]);
    tx.ok();
    for (_, why) in reasons(&tx) {
        assert!(matches!(
            why,
            CandidateReason::NoWeight | CandidateReason::WrongHolding
        ));
    }
    assert!(tx.events::<StrategyPaid>().is_empty());
    // B is paid, then moves to its own second wallet: that wallet still has no weight for p.
    s.pay(p, &[b.pubkey()]).ok();
    let b2 = s.w.wallet_with_sol(SOL);
    let half = s.balance(&b.pubkey()) / 2;
    s.transfer(&b, &b2.pubkey(), half).ok();
    let tx = s.pay(p, &[b.pubkey(), b2.pubkey()]);
    assert_eq!(
        reasons(&tx),
        vec![
            (b.pubkey(), CandidateReason::AlreadyPaid),
            (b2.pubkey(), CandidateReason::NoWeight)
        ]
    );
}

/// A buy and an `enter` in one transaction during the period register nothing for it (the buy's
/// receive registers what was held before: 0); a buy one second before the period counts in full
/// (by design: "held since the period began").
#[test]
fn f2_buying_in_the_period_or_flash_buying_gets_no_weight() {
    let mut s = Strat::new();
    let p = s.round() + 1;
    s.warp_into(p, -1);
    let early = s.buyer(SOL);
    s.warp_into(p, 30);
    let flash = s.w.wallet_with_sol(3 * SOL);
    let ixs = [
        token::create_holding(flash.pubkey(), s.mint, flash.pubkey()),
        s.w.launch_swap_ix(&flash.pubkey(), &s.mint.clone(), 1, 2 * SOL, 0),
        lottery::enter(s.mint, flash.pubkey()),
    ];
    s.w.env.send_paid_by(&ixs, &flash, &[]).ok();
    s.enter(&[&early]);
    assert_eq!(s.weight(&flash.pubkey(), p), 0);
    assert_eq!(s.weight(&early.pubkey(), p), s.balance(&early.pubkey()));
}

/// The same owner is never paid twice for a period: after its receipt is closed (claims ended),
/// the period can no longer be paid at all.
#[test]
fn f3_no_second_payment_once_a_receipt_is_closed() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let p = s.start_round(&[&a]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.plan(p).ok();
    s.pay(p, &[a.pubkey()]).ok();
    s.warp_into(p + 2, 1);
    let ix = companion::close_receipt(s.mint, p, a.pubkey(), s.cranker.pubkey());
    s.send(ix).ok();
    refused(&s.pay(p, &[a.pubkey()]), CompanionError::DrawLate);
    s.enter(&[&a]);
    let _ = s.plan(p + 1);
    let tx = s.pay(p, &[a.pubkey()]);
    tx.expect_fail();
}

/// An extra the strategy reassigns (only it can) stops its own game: plans are rejected
/// (`Accounts`), payments refused (`StrategyAccounts`). Nobody else can swap an extra.
#[test]
fn f4_an_extra_that_changes_owner_only_stops_the_strategy() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let p = s.start_round(&[&a]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.plan(p).ok();
    let mut cfg = s.w.env.account(&config_address(&s.mint)).unwrap();
    cfg.owner = HOOK;
    s.w.env.put(config_address(&s.mint), cfg);
    refused(&s.pay(p, &[a.pubkey()]), CompanionError::StrategyAccounts);
    s.enter(&[&a]);
    s.warp_into(p + 2, 10);
    let tx = s.plan(p + 1);
    tx.ok();
    assert_eq!(tx.event::<PeriodRejected>().reason, AnswerFault::Accounts);
}

/// Evidence for the pool note: a swap and `plan_period` share one transaction with no guard, so a
/// strategy that reads the pool's reserves (the crate's `pool_reserves`) answers on reserves the
/// planner moved in the same transaction.
#[test]
fn f5_a_swap_and_the_plan_share_one_transaction() {
    let mut s = Strat::new();
    let a = s.buyer(SOL);
    let p = s.start_round(&[&a]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    let before = s.w.launch_pool(&s.mint);
    let whale = s.w.wallet_with_sol(30 * SOL);
    let ixs = [
        token::create_holding(whale.pubkey(), s.mint, whale.pubkey()),
        s.w.launch_swap_ix(&whale.pubkey(), &s.mint.clone(), 1, 20 * SOL, 0),
        s.plan_ix(p),
    ];
    let cranker = s.cranker.insecure_clone();
    let tx = s.w.env.send_paid_by(&ixs, &whale, &[&cranker]);
    tx.ok();
    assert!(!tx.events::<PeriodPlanned>().is_empty());
    let after = s.w.launch_pool(&s.mint);
    assert_ne!(
        reserves_of(&before).quote_reserve,
        reserves_of(&after).quote_reserve
    );
    assert_eq!(s.game().status, DrawStatus::Revealed);
}

/// The two-round memory: a holding written again in the period after (the keeper's `enter`, a
/// receive) keeps the period's range in its previous slot, and is paid on it until the payments
/// end (`claims_end` = the end of the period after); a send then cuts it to the balance left.
#[test]
fn f6_a_weight_from_the_previous_slot_is_paid_and_cut_by_sends() {
    let mut s = Strat::new();
    let a = s.buyer(5 * SOL);
    let b = s.buyer(5 * SOL);
    let p = s.start_round(&[&a, &b]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.enter(&[&a, &b]);
    let sa = s.slots(&a.pubkey());
    assert_eq!((sa.current.round, sa.previous.round), (p + 1, p));
    let wb = s.weight(&b.pubkey(), p);
    // B sends 90% away in p + 1: its range of p is cut to what it kept.
    let ninety = s.balance(&b.pubkey()) / 10 * 9;
    let sink = Keypair::new().pubkey();
    s.transfer(&b, &sink, ninety).ok();
    assert_eq!(s.weight(&b.pubkey(), p), s.balance(&b.pubkey()));
    assert!(s.weight(&b.pubkey(), p) < wb / 5);
    s.plan(p).ok();
    let tx = s.pay(p, &[a.pubkey(), b.pubkey()]);
    tx.ok();
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 2);
    // Payments of p end with p + 1.
    s.warp_into(p + 2, 0);
    let c = s.buyer(SOL);
    refused(&s.pay(p, &[c.pubkey()]), CompanionError::DrawLate);
}
