//! Phase 3a audit, round 4 (narrow confirmation of the round-3 fixes: `strategy_class_now` refusing
//! missing or forged class accounts before any terms apply, and `audit_holds` with the
//! `StrategyTerms.audit_ok` / `audit_slot` memo). Report: `bordrless-games-work/log-3a-audit-r4.md`.
//!
//! `r4_f*` tests assert a weakness as it is today; `c_*` tests are controls (an attack tried and
//! refused, or a measurement). The harness is the round-3 one (`audit_3a_r3.rs`), copied.

#![allow(dead_code, unused_imports)]

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateGameArgs, HookStatusArgs, StrategyArgs};
use bordrless_companion::state::{Companion, DrawStatus, Game, GameKind, HookStatus, Split};
use bordrless_core::policy;
use bordrless_game::round_of;
use bordrless_hook::authority::programdata_address;
use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::risk::{risk_label_of, RawAccount, RiskAccounts};
use bordrless_program_tests::timelock::*;
use hook_timelock::loader;
use lottery_hook::client as lottery;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use strategy_tester as st;

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
const HOUR: u32 = 3_600;
const HOOK: Pubkey = lottery_hook::ID;
const STRATEGY: Pubkey = st::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(u32::from(e));
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

fn game_args(round_secs: u32) -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Strategy,
        hook: HOOK,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs,
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
        HookAccountList::new(vec![ExtraAccount {
            writable: false,
            source: AccountSource::Pda {
                program: STRATEGY,
                seeds: vec![Seed::Literal(b"config".to_vec()), Seed::Account(0)],
            },
        }])
        .encode(),
    );
}

fn strategy_world(authority: Option<Pubkey>) -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(STRATEGY, &program_bytes("strategy_tester"))
        .expect("load the tester");
    w.env.set_upgrade_authority(STRATEGY, authority);
    w
}

fn extras(mint: &Pubkey) -> Vec<Pubkey> {
    vec![config_address(mint)]
}

fn hook_status_args(audited: bool, pot_cap: u64, blocked: bool) -> HookStatusArgs {
    HookStatusArgs {
        audited,
        pot_cap,
        blocked,
    }
}

/// A world whose strategy (the tester, Bordrless-managed by Studio's key) is audited with v2 (its
/// hash recorded), and whose ticket hook is audited too: its games are uncapped.
fn audited_world() -> World {
    let mut w = strategy_world(Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &STRATEGY);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(true, 0, false),
                hash,
            )],
            &[&deployer],
        )
        .ok();
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                HOOK,
                hook_status_args(true, 0, false),
            )],
            &[&deployer],
        )
        .ok();
    let s: HookStatus = w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(s.audited && s.audited_hash() == hash);
    w
}

struct Strat {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
    round_secs: u32,
    created: Tx,
}

impl Strat {
    fn with_world(w: World, round_secs: u32) -> Self {
        Self::with_world_lock(w, round_secs, false)
    }

    fn with_world_lock(mut w: World, round_secs: u32, timelocked: bool) -> Self {
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(&mut w, &mint);
        let ixs = vec![
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, round_secs),
            companion::create_strategy_game(
                launcher.pubkey(),
                mint,
                game_args(round_secs),
                strategy_args(),
                vec![],
                timelocked,
                &extras(&mint),
            ),
        ];
        let created = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
        created.ok();
        let (config, tx) = w.create_config(
            &launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: CREATOR_FEE,
                custom_hook: Some(HOOK),
                custom_hook_flags: lottery_hook::FLAGS,
                label: "Strategy".to_string(),
            },
        );
        tx.ok();
        let c = w.launch_config(&config);
        let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, &mint));
        let mut args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
        args.name = "Strategy".to_string();
        let inner = launch::create_launch_with(
            companion::creator_address(&mint),
            mint,
            w.env.treasury.pubkey(),
            w.sol,
            policy::LP_FEE_BPS,
            args.clone(),
            Some(config),
            custom.as_ref(),
        );
        let ix = companion::launch(launcher.pubkey(), mint, &inner, args);
        w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self {
            w,
            mint,
            cranker,
            round_secs,
            created,
        }
    }

    fn r(&self) -> i64 {
        i64::from(self.round_secs)
    }

    fn round(&self) -> u32 {
        round_of(self.w.env.now, self.round_secs)
    }

    fn warp_into(&mut self, round: u32, secs: i64) {
        let t = i64::from(round) * self.r() + secs;
        assert!(t >= self.w.env.now);
        self.w.env.warp(t - self.w.env.now);
    }

    fn send(&mut self, ix: Instruction) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.svm.expire_blockhash();
        self.w.env.send_paid_by(&[ix], &cranker, &[])
    }

    fn send_ixs(&mut self, ixs: &[Instruction]) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.svm.expire_blockhash();
        let mut all = vec![compute_unit_limit(1_400_000)];
        all.extend_from_slice(ixs);
        self.w.env.send_v0(&all, &cranker, &[], &[])
    }

    fn game(&self) -> Game {
        self.w.env.read(&companion::game_address(&self.mint))
    }

    fn companion(&self) -> Companion {
        self.w.env.read(&companion::companion_address(&self.mint))
    }

    fn buyer(&mut self, sol: u64) -> Keypair {
        let t = self.w.wallet_with_sol(sol + SOL);
        self.w.buy(&t, &self.mint, sol).ok();
        t
    }

    fn enter(&mut self, holders: &[&Keypair]) {
        for h in holders {
            let ix = lottery::enter(self.mint, h.pubkey());
            self.send(ix).ok();
        }
    }

    /// Trades round trips and claims the creator fees until the pot holds more than `lamports`.
    fn grow_pot_above(&mut self, lamports: u64) {
        for _ in 0..80 {
            if self.companion().pending_pot > lamports {
                return;
            }
            let t = self.w.wallet_with_sol(21 * SOL);
            self.w.buy(&t, &self.mint, 20 * SOL).ok();
            let held = self.w.env.holding(&self.mint, &t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
            let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
            self.send(ix).ok();
        }
        assert!(self.companion().pending_pot > lamports, "pot did not grow");
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

    fn retire(&mut self) -> Tx {
        let ix = companion::retire_game(self.cranker.pubkey(), self.mint, HOOK);
        self.send(ix)
    }
}

fn without(mut ix: Instruction, key: Pubkey) -> Instruction {
    ix.accounts.retain(|m| m.pubkey != key);
    ix
}

// =============================================================================== round 4 helpers

/// The keeper's own allowance for the companion in a `plan_period` besides the strategy's `plan`
/// (`apps/server/src/keeper/games.ts` `PLAN_BASE_UNITS`): a dry run above
/// `PLAN_BASE_UNITS + plan_cu_max` flags the strategy `over_compute` and leaves the game alone for
/// `STRATEGY_PAUSE_SECS`.
const KEEPER_PLAN_BASE_UNITS: u64 = 80_000;
/// The largest account (and ProgramData) the runtime allows.
const MAX_DATA_LEN: usize = 10 * 1024 * 1024;

/// Compute the strategy's own invocations used (from the runtime's "consumed" logs).
fn strategy_units(tx: &Tx) -> u64 {
    let needle = format!("Program {STRATEGY} consumed ");
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix(&needle))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .sum()
}

fn set_plan_mode(s: &mut Strat, mode: u8, param: u64) {
    let key = config_address(&s.mint);
    put_owned(
        &mut s.w,
        key,
        STRATEGY,
        st::config_bytes(mode, param, st::MODE_PRO_RATA, 0),
    );
}

/// A stranger (no key of the strategy's) grows its ProgramData by `bytes` (`ExtendProgram` is
/// permissionless), then the clock moves a slot (the extension redeploys the code).
fn extend_by_stranger(s: &mut Strat, bytes: u32) {
    s.w.env.warp(1);
    let rent = s.w.env.rent(MAX_DATA_LEN);
    let stranger = s.w.env.funded(rent + 10 * SOL);
    let pd = programdata_address(&STRATEGY);
    let before = programdata_slot(&s.w.env, &STRATEGY);
    s.w.env.svm.expire_blockhash();
    let tx = s.w.env.send_paid_by(
        &[loader::extend_program(
            pd,
            STRATEGY,
            stranger.pubkey(),
            bytes,
        )],
        &stranger,
        &[],
    );
    tx.ok();
    assert_ne!(programdata_slot(&s.w.env, &STRATEGY), before);
    s.w.env.warp(1);
}

fn terms_of(s: &Strat) -> bordrless_companion::state::StrategyTerms {
    s.w.env.read(&companion::strategy_terms_address(&s.mint))
}

/// `plan_period(period)` with the compute limit `units` (as a keeper budgets it).
fn plan_with_limit(s: &mut Strat, period: u32, units: u32) -> Tx {
    let cranker = s.cranker.insecure_clone();
    let ix = s.plan_ix(period);
    s.w.env.svm.expire_blockhash();
    s.w.env
        .send_v0(&[compute_unit_limit(units), ix], &cranker, &[], &[])
}

// =============================================================================== controls

/// Control: anyone's `ExtendProgram` before a plan moves the deploy slot but not the code: the plan
/// recomputes the hash, the audit still holds (no trim), the memo follows the new slot, and the
/// plan after is back to its memo cost. Measures what the recompute costs.
#[test]
fn c_r4_an_extension_before_a_plan_keeps_the_audit() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let pot = s.companion().pending_pot;
    let t = terms_of(&s);
    assert!(t.audit_ok);
    assert_eq!(t.audit_slot, programdata_slot(&s.w.env, &STRATEGY));
    let p = s.round();
    s.warp_into(p + 1, 10);
    let memo = s.send(s.plan_ix(p));
    memo.ok();
    extend_by_stranger(&mut s, 10_240);
    s.warp_into(p + 2, 10);
    let rehash = s.send(s.plan_ix(p + 1));
    rehash.ok();
    assert_eq!(
        rehash.event::<RolledOver>().reason,
        RolloverReason::NoTickets
    );
    assert!(rehash.events::<PotToBuyback>().is_empty());
    assert_eq!(s.companion().pending_pot, pot);
    let t = terms_of(&s);
    assert!(t.audit_ok);
    assert_eq!(t.audit_slot, programdata_slot(&s.w.env, &STRATEGY));
    s.warp_into(p + 3, 10);
    let again = s.send(s.plan_ix(p + 2));
    again.ok();
    let code =
        s.w.env
            .account(&programdata_address(&STRATEGY))
            .unwrap()
            .data
            .len()
            - 45;
    println!(
        "empty-period plan: memo {} CU, after a 10 KiB extension {} CU (+{}), next {} CU; ProgramData code area {} B",
        memo.cu(),
        rehash.cu(),
        rehash.cu() - memo.cu(),
        again.cu(),
        code
    );
    assert!(rehash.cu() > memo.cu() + 20_000, "the hash is recomputed");
    assert!(
        again.cu() < memo.cu() + 2_000,
        "then the memo is used again"
    );
}

/// Control (worst case): a stranger grows the strategy's ProgramData to the 10 MiB maximum. A plan
/// that asks the strategy (burning 70% of its declared 150k) recomputes the hash over the zero
/// tail and still fits easily in a transaction; the audit holds and nothing is trimmed.
#[test]
fn c_r4_a_10_mib_programdata_plans_within_a_transaction() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    set_plan_mode(&mut s, st::MODE_BURN, 105_000);
    let a = s.buyer(5 * SOL);
    let p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    let have =
        s.w.env
            .account(&programdata_address(&STRATEGY))
            .unwrap()
            .data
            .len();
    extend_by_stranger(&mut s, (MAX_DATA_LEN - have) as u32);
    assert_eq!(
        s.w.env
            .account(&programdata_address(&STRATEGY))
            .unwrap()
            .data
            .len(),
        MAX_DATA_LEN
    );
    let pot = s.companion().pending_pot;
    s.warp_into(p + 1, 10);
    let tx = s.send(s.plan_ix(p));
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    let planned: PeriodPlanned = tx.event();
    assert_eq!(planned.pending_pot, pot);
    assert!(terms_of(&s).audit_ok);
    println!(
        "plan at a 10 MiB ProgramData (rehash): {} CU total, strategy {} CU",
        tx.cu(),
        strategy_units(&tx)
    );
    assert!(tx.cu() < 600_000);
}

/// Control: a ProgramData passed twice (and every other account as usual) reads the same: the
/// runtime gives one account per address, so `find` can't be steered to another copy.
#[test]
fn c_r4_a_duplicated_programdata_reads_the_same() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let pot = s.companion().pending_pot;
    let p = s.round();
    s.warp_into(p + 1, 10);
    let mut ix = s.plan_ix(p);
    let pd = programdata_address(&STRATEGY);
    let meta = ix
        .accounts
        .iter()
        .find(|m| m.pubkey == pd)
        .cloned()
        .unwrap();
    ix.accounts.push(meta.clone());
    ix.accounts.push(meta);
    let tx = s.send(ix);
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert_eq!(s.companion().pending_pot, pot);
}

/// Control: an extension between a plan and its payments changes nothing for them: a payment takes
/// the memo (it never hashes), so the audited pot above the cap is not trimmed and the holder is
/// paid; the next plan recomputes and the audit still holds.
#[test]
fn c_r4_an_extension_between_plan_and_pay_changes_nothing() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let a = s.buyer(5 * SOL);
    let p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    s.warp_into(p + 1, 10);
    let planned = s.send(s.plan_ix(p));
    planned.ok();
    let _: PeriodPlanned = planned.event();
    extend_by_stranger(&mut s, 10_240);
    let tx = s.send_ixs(&[s.pay_ix(p, &[a.pubkey()])]);
    tx.ok();
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 1);
    assert!(s.companion().pending_pot > DEFAULT_POT_CAP);
    s.warp_into(p + 2, 10);
    let next = s.send(s.plan_ix(p + 1));
    next.ok();
    assert!(next.events::<PotToBuyback>().is_empty());
    assert!(terms_of(&s).audit_ok);
}

// =============================================================================== R4-F1

const STARTER: Pubkey = strategy_pro_rata::ID;

/// Studio's pro-rata starter, Bordrless-managed (Studio's key) and audited with v2, as the strategy
/// of a launched game with `plan_cu_max` declared; answers the game and the starter's state.
fn starter_game(plan_cu_max: u32) -> (Strat, Pubkey) {
    use anchor_lang::{InstructionData, ToAccountMetas};
    let mut w = World::new();
    w.env
        .svm
        .add_program(STARTER, &program_bytes("strategy_pro_rata"))
        .expect("load the starter");
    w.env.set_upgrade_authority(STARTER, Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &STARTER);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STARTER,
                hook_status_args(true, 0, false),
                hash,
            )],
            &[&deployer],
        )
        .ok();
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let state =
        Pubkey::find_program_address(&[strategy_pro_rata::STATE_SEED, mint.as_ref()], &STARTER).0;
    let prepare = Instruction {
        program_id: STARTER,
        accounts: strategy_pro_rata::accounts::Prepare {
            payer: launcher.pubkey(),
            mint,
            state,
            registry: bordrless_strategy::registry_address(&STARTER, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: strategy_pro_rata::instruction::Prepare {
            budget_bps_of_max: 10_000,
        }
        .data(),
    };
    let s = StrategyArgs {
        strategy: STARTER,
        plan_cu_max,
        ..strategy_args()
    };
    let ixs = vec![
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, HOUR),
        prepare,
        companion::create_strategy_game(
            launcher.pubkey(),
            mint,
            game_args(HOUR),
            s,
            vec![],
            false,
            &[state],
        ),
    ];
    let created = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
    created.ok();
    let set: StrategySet = created.event();
    assert_eq!(
        set.class,
        strategy_class::STATUS,
        "a status: its class is STATUS"
    );
    let (config, tx) = w.create_config(
        &launcher,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(HOOK),
            custom_hook_flags: lottery_hook::FLAGS,
            label: "Strategy".to_string(),
        },
    );
    tx.ok();
    let c = w.launch_config(&config);
    let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, &mint));
    let mut args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
    args.name = "Strategy".to_string();
    let inner = launch::create_launch_with(
        companion::creator_address(&mint),
        mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        Some(config),
        custom.as_ref(),
    );
    let ix = companion::launch(launcher.pubkey(), mint, &inner, args);
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    w.env.warp(31);
    let cranker = w.wallet_with_sol(5 * SOL);
    (
        Strat {
            w,
            mint,
            cranker,
            round_secs: HOUR,
            created,
        },
        state,
    )
}

fn starter_plan(s: &mut Strat, state: Pubkey, period: u32, units: u32) -> Tx {
    let cranker = s.cranker.insecure_clone();
    let ix = companion::plan_period(
        cranker.pubkey(),
        s.mint,
        HOOK,
        STARTER,
        s.w.launch(&s.mint).pool,
        &[state],
        period,
    );
    s.w.env.svm.expire_blockhash();
    s.w.env
        .send_v0(&[compute_unit_limit(units), ix], &cranker, &[], &[])
}

fn units_of(tx: &Tx, program: &Pubkey) -> u64 {
    let needle = format!("Program {program} consumed ");
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix(&needle))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .sum()
}

/// R4-F1. After anyone's `ExtendProgram` (permissionless on a Bordrless-managed strategy; ~0.07 SOL
/// of rent at the 10 KiB minimum), the next `plan_period` recomputes the code's hash: ~0.5 CU a
/// byte of code plus the zero-tail scan, ~70k for Studio's own 142 KB starter. The keeper budgets a
/// plan at `PLAN_BASE_UNITS` (80k) + `plan_cu_max`, refuses (flags `over_compute`, pauses 6 h) any
/// dry run above it, and so never sends it: the memo is never refreshed and every later dry run is
/// over again. A starter game declaring a `plan_cu_max` its plan fits at 70% (Studio's simulator
/// rule) plans within the keeper's budget before the extension and never after: the honest keeper
/// stops planning the game for good; its periods go unplanned and the pot retires to the buyback
/// after 60 dormant days. Shown with the keeper's budget as the transaction's compute limit; a
/// sender giving it more compute unblocks it once (the memo follows the new slot).
#[test]
fn r4_f1_one_extension_puts_every_later_plan_over_the_keepers_budget() {
    // The starter's plan uses about 5.6k: declared 12k (it runs at under half of it).
    let plan_cu_max = 12_000u32;
    let (mut s, state) = starter_game(plan_cu_max);
    let set: StrategySet = s.created.event();
    assert!(set.audited || set.pot_cap == DEFAULT_POT_CAP);
    assert!(
        terms_of(&s).audit_ok,
        "the starter's audit holds at creation"
    );
    s.grow_pot_above(MIN_POT);
    let cap = (KEEPER_PLAN_BASE_UNITS + u64::from(plan_cu_max)) as u32;
    let a = s.buyer(5 * SOL);
    let p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    s.warp_into(p + 1, 5);
    s.enter(&[&a]);
    let before = starter_plan(&mut s, state, p, cap);
    before.ok();
    let _: PeriodPlanned = before.event();
    let used = units_of(&before, &STARTER);
    println!(
        "starter plan before: {} CU of the keeper's {cap} (strategy {used} CU = {}% of plan_cu_max {plan_cu_max})",
        before.cu(),
        used * 100 / u64::from(plan_cu_max)
    );
    assert!(
        used * 10 <= u64::from(plan_cu_max) * 7,
        "within Studio's 70%"
    );
    // A stranger extends the starter's ProgramData by the 10 KiB minimum.
    {
        s.w.env.warp(1);
        let stranger = s.w.env.funded(10 * SOL);
        let pd = programdata_address(&STARTER);
        s.w.env.svm.expire_blockhash();
        s.w.env
            .send_paid_by(
                &[loader::extend_program(
                    pd,
                    STARTER,
                    stranger.pubkey(),
                    10_240,
                )],
                &stranger,
                &[],
            )
            .ok();
        s.w.env.warp(1);
    }
    // Every later plan within the keeper's budget runs out of compute.
    for q in [p + 1, p + 2] {
        s.warp_into(q + 1, 5);
        s.enter(&[&a]);
        let tx = starter_plan(&mut s, state, q, cap);
        tx.expect_fail();
        assert!(
            tx.logs()
                .iter()
                .any(|l| l.contains("Computational budget exceeded")),
            "out of compute:\n{}",
            tx.logs().join("\n")
        );
    }
    // What it needs now (the full 1.4M): over the keeper's budget, by the hash.
    let q = p + 3;
    s.warp_into(q + 1, 5);
    s.enter(&[&a]);
    let full = starter_plan(&mut s, state, q, 1_400_000);
    full.ok();
    println!(
        "starter plan after the extension: {} CU (keeper's budget {cap}); strategy {} CU",
        full.cu(),
        units_of(&full, &STARTER)
    );
    assert!(u64::from(full.cu() as u32) > u64::from(cap));
    // Once someone plans with more compute, the memo follows and the keeper's budget suffices.
    assert!(terms_of(&s).audit_ok);
    let q = p + 4;
    s.warp_into(q + 1, 5);
    s.enter(&[&a]);
    starter_plan(&mut s, state, q, cap).ok();
}

// =============================================================================== Info

/// Info. A stale audit (Studio upgraded the code, no un-audit yet) makes every plan recompute the
/// hash: it never holds, so the memo is never set. Measured on empty periods.
#[test]
fn c_r4_a_stale_audit_rehashes_at_every_plan() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(MIN_POT);
    let new_code = program_bytes("strategy_pro_rata");
    let pd = programdata_address(&STRATEGY);
    let have = s.w.env.account(&pd).unwrap().data.len() - 45;
    let grow = (new_code.len().saturating_sub(have) as u32).max(10_240);
    let buffer = Pubkey::new_unique();
    put_buffer(&mut s.w.env, buffer, STUDIO_KEY, &new_code, 0);
    let payer = s.w.env.payer.pubkey();
    s.w.env.without_sigverify();
    s.w.env.svm.expire_blockhash();
    send_as(
        &mut s.w.env,
        &[loader::extend_program(pd, STRATEGY, payer, grow)],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    s.w.env.warp(1);
    send_as(
        &mut s.w.env,
        &[loader::upgrade(pd, STRATEGY, buffer, payer, STUDIO_KEY)],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    s.w.env.warp(1);
    let p = s.round();
    let mut cus = vec![];
    for q in [p, p + 1, p + 2] {
        s.warp_into(q + 1, 10);
        let tx = s.send(s.plan_ix(q));
        tx.ok();
        assert!(!terms_of(&s).audit_ok);
        cus.push(tx.cu());
    }
    println!("empty-period plans on a stale audit: {cus:?} CU (each recomputes the hash)");
    assert!(cus[2] + 1_000 > cus[1] && cus[1] + 1_000 > cus[2]);
}

// =============================================================================== Fix 7 (independent audit)

/// Independent audit, integration 7 (fixed). The pro-rata starter kept no bump: its `plan` and
/// `entitle` checked their state's address with Anchor's `seeds…, bump`, a bump search whose cost
/// (about 1,500 units a try) depends on the mint, so the same starter used more compute on some
/// mints than others (and `r4_f1`'s 70% check failed on unlucky mints). It now stores the bump at
/// `prepare` and checks with it: `plan` and `entitle` cost the same on every mint. Swept over 32
/// fresh mints, each prepared and asked directly.
#[test]
fn starter_compute_is_the_same_for_every_mint() {
    use anchor_lang::solana_program::instruction::AccountMeta;
    use anchor_lang::{InstructionData, ToAccountMetas};
    use bordrless_strategy::{EntitleArgs, PlanArgs, ARGS_VERSION};
    let mut w = World::new();
    w.env
        .svm
        .add_program(STARTER, &program_bytes("strategy_pro_rata"))
        .expect("load the starter");
    let payer = w.wallet_with_sol(50 * SOL);
    let prefix = |n: usize| -> Vec<AccountMeta> {
        (0..n)
            .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
            .collect()
    };
    let (mut plans, mut entitles) = (Vec::new(), Vec::new());
    for _ in 0..32 {
        let mint = Keypair::new();
        let m = mint.pubkey();
        let state =
            Pubkey::find_program_address(&[strategy_pro_rata::STATE_SEED, m.as_ref()], &STARTER).0;
        let prepare = Instruction {
            program_id: STARTER,
            accounts: strategy_pro_rata::accounts::Prepare {
                payer: payer.pubkey(),
                mint: m,
                state,
                registry: bordrless_strategy::registry_address(&STARTER, &m).0,
                system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: strategy_pro_rata::instruction::Prepare {
                budget_bps_of_max: 10_000,
            }
            .data(),
        };
        w.env.send_paid_by(&[prepare], &payer, &[&mint]).ok();
        let mut accounts = prefix(5);
        accounts.push(AccountMeta::new_readonly(state, false));
        let plan = Instruction {
            program_id: STARTER,
            accounts: accounts.clone(),
            data: strategy_pro_rata::instruction::Plan {
                args: PlanArgs {
                    version: ARGS_VERSION,
                    mint: m,
                    total: 1_000,
                    pot: SOL,
                    budget_max: SOL / 2,
                    ..PlanArgs::default()
                },
            }
            .data(),
        };
        let tx = w.env.send_bare(&[plan], &[]);
        tx.ok();
        plans.push(units_of(&tx, &STARTER));
        let entitle = Instruction {
            program_id: STARTER,
            accounts,
            data: strategy_pro_rata::instruction::Entitle {
                args: EntitleArgs {
                    version: ARGS_VERSION,
                    mint: m,
                    owner: Pubkey::new_unique(),
                    balance: 100,
                    weight: 100,
                    total: 1_000,
                    budget: SOL / 2,
                    max_amount: SOL / 8,
                    ..EntitleArgs::default()
                },
            }
            .data(),
        };
        w.env.svm.expire_blockhash();
        let tx = w.env.send_bare(&[entitle], &[]);
        tx.ok();
        entitles.push(units_of(&tx, &STARTER));
    }
    let spread = |v: &[u64]| v.iter().max().unwrap() - v.iter().min().unwrap();
    println!(
        "starter over 32 mints: plan {}..{} CU, entitle {}..{} CU",
        plans.iter().min().unwrap(),
        plans.iter().max().unwrap(),
        entitles.iter().min().unwrap(),
        entitles.iter().max().unwrap()
    );
    assert_eq!(spread(&plans), 0, "plan: {plans:?}");
    assert_eq!(spread(&entitles), 0, "entitle: {entitles:?}");
}
