//! Phase 3a audit, round 2: custody, manipulation and authority (on-chain changes, not the vault).
//! Report: `bordrless-games-work/log-3a-audit-r2.md`.
//!
//! Each finding has a PoC named `r2_f<N>_...` that asserts the weakness as it is today (a fix flips
//! it); each `c_...` test is a control: an attack tried against a round-1 fix and refused.
//!
//! The strategy harness is the round-1 one (`audit_3a_manip_r1.rs`) with the period length a
//! parameter; scripted strategy behaviour comes only from the test-only tester's config account.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{
    AttestArgs, CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs, StrategyArgs,
};
use bordrless_companion::state::{
    Companion, DrawStatus, Game, GameKind, HookAttestation, HookStatus, Split,
};
use bordrless_core::policy;
use bordrless_game::round_of;
use bordrless_hook::authority::{
    classify_programdata, programdata_address, trimmed_len, trimmed_len_by, AuthorityClass,
    MIN_ACCEPTED_DELAY, TRIM_CHUNK_MAX, TRIM_CHUNK_MIN,
};
use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::*;
use hook_timelock::client as tl;
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
const WEEK: u32 = 7 * 86_400;
const HOOK: Pubkey = lottery_hook::ID;
const STRATEGY: Pubkey = st::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const ATTESTER: Pubkey = STUDIO_ATTESTER;
const JACKPOT: Pubkey = studio_jackpot::ID;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
}

// ------------------------------------------------------------------------------------- strategy

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

/// The setup of a strategy game for a fresh mint in `w`: answers the transaction and the mint.
fn setup_game(w: &mut World, round_secs: u32, timelocked: bool) -> (Tx, Keypair, Keypair) {
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    prepare_strategy(w, &mint);
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
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
    (tx, launcher, mint_kp)
}

struct Strat {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
    round_secs: u32,
}

impl Strat {
    /// A strategy coin in `w` (statuses or authorities set up before), launched and 31 s on.
    fn with_world(mut w: World, round_secs: u32, timelocked: bool) -> Self {
        let (tx, launcher, mint_kp) = setup_game(&mut w, round_secs, timelocked);
        tx.ok();
        let mint = mint_kp.pubkey();
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

    fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
        let next = self.round() + 1;
        self.warp_into(next, 5);
        self.enter(holders);
        next
    }

    fn fund_pot(&mut self) {
        let t = self.w.wallet_with_sol(21 * SOL);
        self.w.buy(&t, &self.mint, 20 * SOL).ok();
        let held = self.w.env.holding(&self.mint, &t.pubkey());
        self.w.sell(&t, &self.mint, held).ok();
        let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
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

    fn send_ixs(&mut self, ixs: &[Instruction]) -> Tx {
        let cranker = self.cranker.insecure_clone();
        self.w.env.svm.expire_blockhash();
        let mut all = vec![compute_unit_limit(1_400_000)];
        all.extend_from_slice(ixs);
        self.w.env.send_v0(&all, &cranker, &[], &[])
    }

    fn pay(&mut self, period: u32, owners: &[Pubkey]) -> Tx {
        let ix = self.pay_ix(period, owners);
        self.send_ixs(&[ix])
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

fn hook_status_args(audited: bool, pot_cap: u64, blocked: bool) -> HookStatusArgs {
    HookStatusArgs {
        audited,
        pot_cap,
        blocked,
    }
}

// =============================================================================== R2-F1

/// R2-F1 (round-1 M-5 fix incomplete). The `STRATEGY_ACTIVE_BPS` floor stops dust payments from
/// refreshing `settled_at`, but `retire` is held back by something else: it needs the game `Idle`
/// (a planned period is `Revealed` until its claims end, which is when the next period becomes
/// plannable) and no plannable period (`prize_due`). So a strategy whose every period is planned,
/// with any budget (here 1,000 lamports, paying 1 lamport), can never be retired: there is no
/// instant between "period p open" and "period p+1 due". The protocol's own keeper plans every
/// period, so the strategy's author doesn't even have to crank. The pot (here ~0.5 SOL; up to the
/// 10 SOL cap, unbounded when audited) is never paid to holders nor retired to the buyback.
#[test]
fn r2_f1_planning_every_period_keeps_a_dust_strategys_pot_from_retiring() {
    let mut s = Strat::with_world(strategy_world(Some(STUDIO_KEY)), WEEK, false);
    let launched = s.companion().launched_at;
    let a = s.buyer(SOL);
    let mut p = s.start_round(&[&a]);
    s.fund_pot();
    s.set_config((st::MODE_FLAT, 1_000), (st::MODE_FLAT, 1));
    let retirable = s.game().retirable_at(launched);
    assert_eq!(retirable, launched + 2 * DORMANT_SECS);
    // Weekly periods, each planned and paid one lamport (what keepers and the author do), until
    // the game is past retirable.
    while s.w.env.now < retirable {
        s.warp_into(p + 1, 5);
        s.enter(&[&a]);
        let tx = s.plan(p);
        tx.ok();
        assert_eq!(tx.event::<PeriodPlanned>().budget, 1_000);
        let tx = s.pay(p, &[a.pubkey()]);
        tx.ok();
        assert_eq!(tx.event::<StrategyPaid>().payments[0].amount, 1);
        p += 1;
    }
    // Fixed (round 2): the dust never counted as activity, and past its retirement a strategy's
    // open period is closed by `retire` (its plans don't hold it back): the pot goes to the buyback.
    assert_eq!(s.game().settled_at, 0);
    assert_eq!(s.game().status, DrawStatus::Revealed);
    let pot = s.companion().pending_pot;
    assert!(pot > MIN_POT, "pot {pot}");
    s.retire().ok();
    assert_eq!(s.companion().pending_pot, 0);
    assert_eq!(s.companion().pot_locked, 0);
    assert_eq!(s.game().status, DrawStatus::Idle);
}

// =============================================================================== R2-F2

/// R2-F2. An audit is final on chain (`write_status`: no block once audited, no un-audit), and a
/// strategy's audit counts whenever it has a hash (`read_strategy_terms`), whatever code now runs
/// and whoever can change it. Studio's key hands a Bordrless-managed strategy, audited with v2, to
/// its creator's timelock (owner decision 3: "timelocked-by-me"); three days later the creator's
/// other code runs at the strategy's address. The status still says audited: a new game on it is
/// uncapped (`StrategySet { audited: true, pot_cap: 0 }`), and the protocol can neither block it,
/// cap it, nor re-audit it (`BadHookStatus`, `AuditNeedsFixedCode`). With the lottery hook audited
/// (as on mainnet, so it can't be blocked either), nothing on chain can stop or cap the new code.
#[test]
fn r2_f2_an_audit_outlives_a_handover_to_a_timelock_and_new_code() {
    let mut w = strategy_world(Some(STUDIO_KEY));
    w.env.without_sigverify();
    let deployer = w.env.deployer.insecure_clone();
    // The protocol audits the strategy (v2, its hash recorded) and the lottery hook.
    let audited_hash = programdata_hash(&w.env, &STRATEGY);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(true, 0, false),
                audited_hash,
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
    // A game made now is uncapped: as intended, for the audited code.
    let (tx, _, _) = setup_game(&mut w, HOUR, false);
    tx.ok();
    let set: StrategySet = tx.event();
    assert_eq!((set.audited, set.pot_cap), (true, 0));
    // Studio's key hands the program to its creator's timelock (the key signs; sigverify off).
    let author = w.env.funded(100 * SOL);
    let payer = w.env.payer.pubkey();
    send_as(
        &mut w.env,
        &[tl::register(payer, STUDIO_KEY, STRATEGY, MIN_ACCEPTED_DELAY, author.pubkey())],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    // The creator proposes other code (here Studio's pro-rata starter's build, standing for any
    // code nobody audited); anyone grows the ProgramData and executes it once the delay is over.
    let new_code = program_bytes("strategy_pro_rata");
    let buffer = Pubkey::new_unique();
    put_buffer(&mut w.env, buffer, author.pubkey(), &new_code, 0);
    w.env
        .send(
            &[loader::set_authority(
                buffer,
                author.pubkey(),
                Some(timelock_of(&STRATEGY)),
            )],
            &[&author],
        )
        .ok();
    w.env
        .send(
            &[tl::propose(
                author.pubkey(),
                STRATEGY,
                buffer,
                trimmed_len(&new_code) as u32,
            )],
            &[&author],
        )
        .ok();
    let have = w.env.account(&programdata_address(&STRATEGY)).unwrap().data.len() - 45;
    let grow = (new_code.len().saturating_sub(have) as u32).max(10_240);
    w.env.warp(1);
    w.env
        .send(
            &[loader::extend_program(
                programdata_address(&STRATEGY),
                STRATEGY,
                author.pubkey(),
                grow,
            )],
            &[&author],
        )
        .ok();
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY) + 1);
    w.env
        .send(
            &[tl::execute(author.pubkey(), STRATEGY, buffer, author.pubkey())],
            &[&author],
        )
        .ok();
    let now_hash = programdata_hash(&w.env, &STRATEGY);
    assert_ne!(now_hash, audited_hash);
    assert_eq!(upgrade_authority(&w.env, &STRATEGY), Some(timelock_of(&STRATEGY)));
    let status: HookStatus = w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(status.audited);
    assert_eq!(status.audited_hash(), audited_hash);
    // Fixed (round 2): a strategy's audit counts only while it is immutable or Bordrless-managed,
    // so a new game on the replaced code is capped.
    let (tx, _, _) = setup_game(&mut w, HOUR, true);
    tx.ok();
    let set: StrategySet = tx.event();
    assert_eq!(
        (set.class, set.audited, set.pot_cap),
        (strategy_class::STATUS, false, DEFAULT_POT_CAP)
    );
    // v1 still can't touch an audit; v2 lifts it once it shows it stale (the program no longer
    // auditable), and caps it; a re-audit of the new code is still refused.
    w.env.warp(1);
    refused(
        &w.env.send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, DEFAULT_POT_CAP, true),
            )],
            &[&deployer],
        ),
        CompanionError::BadHookStatus,
    );
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, MIN_POT_CAP, false),
                [0; 32],
            )],
            &[&deployer],
        )
        .ok();
    let status: HookStatus = w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(!status.audited && status.pot_cap == MIN_POT_CAP);
    assert_eq!(status.audited_hash(), [0; 32]);
    refused(
        &w.env.send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(true, 0, false),
                now_hash,
            )],
            &[&deployer],
        ),
        CompanionError::AuditNeedsFixedCode,
    );
    // The ticket hook's audit (v1, no hash recorded) can be lifted and the hook blocked with v2.
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                HOOK,
                hook_status_args(false, DEFAULT_POT_CAP, true),
                [0; 32],
            )],
            &[&deployer],
        )
        .ok();
    let status: HookStatus = w.env.read(&companion::hook_status_address(&HOOK));
    assert!(!status.audited && status.blocked);
}

// =============================================================================== R2-F3

/// R2-F3. Round 1 made a v1 audit (no hash) count as no audit for a strategy's cap
/// (`read_strategy_terms`), but `write_status` still treats the status as audited: final, never
/// blockable. A Bordrless-managed strategy given a v1 audit (the deployed client, or by mistake)
/// is capped like any unaudited strategy, yet the protocol has lost the switch every unaudited
/// strategy keeps: it can never be blocked, nor its status lowered, nor re-audited with v2 once it
/// is no longer auditable.
#[test]
fn r2_f3_a_v1_audited_strategy_counts_as_unaudited_but_can_never_be_blocked() {
    let mut w = strategy_world(Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(true, 0, false),
            )],
            &[&deployer],
        )
        .ok();
    let (tx, _, _) = setup_game(&mut w, HOUR, false);
    tx.ok();
    let set: StrategySet = tx.event();
    // Capped as unaudited ...
    assert_eq!(
        (set.class, set.audited, set.pot_cap),
        (strategy_class::STATUS, false, DEFAULT_POT_CAP)
    );
    // ... v1 can't block it ("an audit is final"), but (fixed, round 2) v2 lifts an audit that
    // recorded no hash and blocks it.
    w.env.warp(1);
    refused(
        &w.env.send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, DEFAULT_POT_CAP, true),
            )],
            &[&deployer],
        ),
        CompanionError::BadHookStatus,
    );
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, DEFAULT_POT_CAP, true),
                [0; 32],
            )],
            &[&deployer],
        )
        .ok();
    let status: HookStatus = w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(!status.audited && status.blocked);
    // An unaudited strategy can be blocked (the control).
    let mut w = strategy_world(Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, DEFAULT_POT_CAP, true),
            )],
            &[&deployer],
        )
        .ok();
}

// =============================================================================== R2-F4

fn att_world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(JACKPOT, &program_bytes("studio_jackpot"))
        .expect("load");
    w.env.set_upgrade_authority(JACKPOT, Some(STUDIO_KEY));
    w.env.without_sigverify();
    w.env.fund(ATTESTER, 10 * SOL);
    w.env.warp(1);
    w
}

fn attest_now(env: &mut Env, program: Pubkey) -> Tx {
    let a = AttestArgs {
        build_hash: programdata_hash(env, &program),
        source_hash: [5; 32],
        template_commit: [6; 20],
        sim_version: 3,
        sim_pass: true,
        cut_max_bps: 0,
        cap_bps: 0,
        review: REVIEW_PASS,
        kind: ATTEST_KIND_GAME_HOOK,
    };
    env.svm.expire_blockhash();
    send_as(env, &[companion::attest(ATTESTER, program, a)], &[], &[ATTESTER])
}

/// A jackpot game on the Studio starter (its vetting accounts passed).
fn make_jackpot_game(w: &mut World) -> Tx {
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let args = CreateGameArgs {
        kind: GameKind::Jackpot,
        hook: JACKPOT,
        split: SPLIT,
        pot_bps: POT_BPS,
        round_secs: 0,
        min_pot: MIN_POT,
        prize_bps: 5_000,
        claim_window_secs: 0,
        max_attempts: 0,
    };
    let k = GameKindArgs {
        timer_secs: studio_jackpot::TIMER_SECS,
        min_tokens: studio_jackpot::MIN_TOKENS,
        ..GameKindArgs::default()
    };
    let prepare = Instruction {
        program_id: JACKPOT,
        accounts: studio_jackpot::accounts::Prepare {
            payer: launcher.pubkey(),
            mint,
            state: bordrless_game::state_address(&JACKPOT, &mint).0,
            registry: bordrless_hook::hook_accounts_address(&JACKPOT, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: studio_jackpot::instruction::Prepare {}.data(),
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare,
        companion::create_game_v2_attested(launcher.pubkey(), mint, args, k, false),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
}

/// Studio's key upgrades `program` to `code` (the loader's `Upgrade`, the key signing with
/// sigverify off), then the clock moves a slot.
fn studio_upgrade(env: &mut Env, program: Pubkey, code: &[u8]) {
    let buffer = Pubkey::new_unique();
    put_buffer(env, buffer, STUDIO_KEY, code, 0);
    let spill = env.payer.pubkey();
    env.svm.expire_blockhash();
    send_as(
        env,
        &[loader::upgrade(
            programdata_address(&program),
            program,
            buffer,
            spill,
            STUDIO_KEY,
        )],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    env.warp(1);
}

/// R2-F4 (round-1 F2 fix incomplete). A protocol revocation sticks to the revoked build only while
/// it is the attestation's last record: `attest` compares the new hash with `a.build_hash` alone and
/// clears the mark (`reserved = [0; 32]`) on any other build. Whoever holds the attester key and
/// the program's upgrade authority (Studio's build worker, the threat the round-1 fix answers)
/// upgrades to any other build, attests it, upgrades back and attests the revoked build again: two
/// upgrades (seconds for a Studio-managed hook; two public delays for a timelocked one).
#[test]
fn r2_f4_a_protocol_revocation_is_laundered_by_an_upgrade_round_trip() {
    let mut w = att_world();
    let build_a = program_bytes("studio_jackpot");
    attest_now(&mut w.env, JACKPOT).ok();
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send(&[companion::revoke(deployer.pubkey(), JACKPOT)], &[&deployer])
        .ok();
    make_jackpot_game(&mut w).expect_code(code(CompanionError::HookNotAttested));
    w.env.warp(1);
    attest_now(&mut w.env, JACKPOT).expect_code(code(CompanionError::RevokedByProtocol));
    // Fixed (round 2): the protocol's revocation is a hold on the program: build B is refused,
    // and so is build A again.
    studio_upgrade(&mut w.env, JACKPOT, &program_bytes("strategy_tester"));
    attest_now(&mut w.env, JACKPOT).expect_code(code(CompanionError::RevokedByProtocol));
    studio_upgrade(&mut w.env, JACKPOT, &build_a);
    assert_eq!(programdata_hash(&w.env, &JACKPOT), executable_hash(&build_a));
    attest_now(&mut w.env, JACKPOT).expect_code(code(CompanionError::RevokedByProtocol));
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert!(at.revoked);
    make_jackpot_game(&mut w).expect_code(code(CompanionError::HookNotAttested));
}

// =============================================================================== R2-F5

/// R2-F5 (Studio's starter, round-1 M-8 follow-up). The pro-rata starter's `prepare` now needs the
/// mint's signature, but it creates its registry with a bare `create_account`, which fails on an
/// address that already holds lamports. Anyone who knows the mint address (from the first of the
/// two setup transactions the integration review found necessary, or a leaked one) sends the
/// rent-exempt minimum (0.00089 SOL) to `PDA(["bordrless-strategy-accounts", mint], starter)` first, and that mint can never
/// be prepared: the launcher has to start over with a new mint. `bordrless_hook::write_registry`
/// (what `lottery_hook::prepare` uses) takes a funded address over; the starter, "what a Studio
/// strategy is", doesn't.
#[test]
fn r2_f5_a_transfer_to_the_starters_registry_blocks_its_prepare() {
    const STARTER: Pubkey = strategy_pro_rata::ID;
    let mut w = World::new();
    w.env
        .svm
        .add_program(STARTER, &program_bytes("strategy_pro_rata"))
        .expect("load the starter");
    w.env.set_upgrade_authority(STARTER, None);
    let prepare = |payer: Pubkey, mint: Pubkey| Instruction {
        program_id: STARTER,
        accounts: strategy_pro_rata::accounts::Prepare {
            payer,
            mint,
            state: Pubkey::find_program_address(
                &[strategy_pro_rata::STATE_SEED, mint.as_ref()],
                &STARTER,
            )
            .0,
            registry: bordrless_strategy::registry_address(&STARTER, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: strategy_pro_rata::instruction::Prepare {
            budget_bps_of_max: 10_000,
        }
        .data(),
    };
    let launcher = w.wallet_with_sol(5 * SOL);
    // Control: a fresh mint prepares.
    let ok_mint = Keypair::new();
    w.env
        .send_paid_by(&[prepare(launcher.pubkey(), ok_mint.pubkey())], &launcher, &[&ok_mint])
        .ok();
    // The griefer's transfer, then the launcher's (mint-signed) prepare: refused for good.
    let mint_kp = Keypair::new();
    let registry = bordrless_strategy::registry_address(&STARTER, &mint_kp.pubkey()).0;
    // A plain transfer of the rent-exempt minimum of an empty account (0.00089 SOL).
    let griefer = w.wallet_with_sol(SOL);
    let lamports = w.env.rent(0);
    w.env
        .send_paid_by(
            &[anchor_lang::solana_program::system_instruction::transfer(
                &griefer.pubkey(),
                &registry,
                lamports,
            )],
            &griefer,
            &[],
        )
        .ok();
    // Fixed (round 2): the starter takes a funded address over (top up, allocate, assign).
    w.env
        .send_paid_by(&[prepare(launcher.pubkey(), mint_kp.pubkey())], &launcher, &[&mint_kp])
        .ok();
    assert_eq!(w.env.account(&registry).unwrap().owner, STARTER);
}

// =============================================================================== controls

/// `trimmed_len_by` (any `eq`) against the plain scan: the last nonzero byte at and around every
/// chunk boundary the doubling and halving visit (32 * 2^k ± 1, up to 1 MiB and past it), code
/// shorter than a chunk, all-zero code, and nonzero bytes inside the zero tail's first chunk. A
/// counting `eq` bounds the work: at most ~2 compared bytes per byte of zero tail.
#[test]
fn c_trimmed_len_is_the_plain_scan_at_every_chunk_boundary() {
    let naive = |c: &[u8]| c.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    let mut checked = 0;
    for len in [0usize, 1, 2, 31, 32, 33, 64, 65, 96, 97, 4_096, 65_537, (1 << 20) + 33, 3 << 20] {
        let mut positions: Vec<usize> = (0..len.min(70)).collect();
        positions.extend(len.saturating_sub(70)..len);
        let mut k = TRIM_CHUNK_MIN;
        while k <= TRIM_CHUNK_MAX * 2 {
            for d in [k - 1, k, k + 1, 2 * k - 1, 2 * k + 1, 3 * k] {
                if d < len {
                    positions.push(len - 1 - d);
                    positions.push(d);
                }
            }
            k *= 2;
        }
        positions.sort_unstable();
        positions.dedup();
        let mut w = vec![0u8; len];
        assert_eq!(trimmed_len(&w), 0);
        for &at in &positions {
            w[at] = 1;
            let compared = std::cell::Cell::new(0usize);
            let got = trimmed_len_by(&w, |a, b| {
                compared.set(compared.get() + a.len());
                a == b
            });
            assert_eq!(got, naive(&w), "len {len}, nonzero at {at}");
            let tail = len - got;
            assert!(
                compared.get() <= 2 * tail + 2 * TRIM_CHUNK_MAX,
                "len {len} at {at}: compared {} for a tail of {tail}",
                compared.get()
            );
            // A second nonzero just inside the first chunk of the tail.
            if at + 40 < len {
                w[at + 40] = 2;
                assert_eq!(trimmed_len(&w), naive(&w), "len {len}, at {at} and {}", at + 40);
                w[at + 40] = 0;
            }
            w[at] = 0;
            checked += 1;
        }
    }
    assert!(checked > 1_000);
}

/// The strategy's class is read again before every question from its ProgramData and `Timelock`,
/// looked up by address among all accounts. Nothing a sender passes changes the class: leaving
/// the `Timelock` or the ProgramData out, or passing another program's in their place, is refused
/// (`StrategyNotAccepted`), and a system account with lamports at the timelock's address (all an
/// outsider can create there) isn't read as one.
#[test]
fn c_the_strategys_class_accounts_cant_be_left_out_or_swapped() {
    let author = Keypair::new();
    let mut w = strategy_world(Some(author.pubkey()));
    w.env.fund(author.pubkey(), 10 * SOL);
    register(&mut w.env, &author, STRATEGY, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    let mut s = Strat::with_world(w, HOUR, true);
    let a = s.buyer(5 * SOL);
    let p = s.start_round(&[&a]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    let pd = programdata_address(&STRATEGY);
    let lock = timelock_of(&STRATEGY);
    let without = |mut ix: Instruction, key: Pubkey| {
        ix.accounts.retain(|m| m.pubkey != key);
        ix
    };
    let swapped = |mut ix: Instruction, key: Pubkey, by: Pubkey| {
        for m in ix.accounts.iter_mut().filter(|m| m.pubkey == key) {
            m.pubkey = by;
        }
        ix
    };
    let ix = without(s.plan_ix(p), lock);
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    let ix = without(s.plan_ix(p), pd);
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    // Another timelocked program's Timelock and ProgramData (the lottery hook's ProgramData; a
    // timelock of tax_hook) in their places.
    let other_author = s.w.env.funded(10 * SOL);
    s.w.env
        .set_upgrade_authority(tax_hook::ID, Some(other_author.pubkey()));
    register(&mut s.w.env, &other_author, tax_hook::ID, MIN_ACCEPTED_DELAY, other_author.pubkey())
        .ok();
    let ix = swapped(s.plan_ix(p), lock, timelock_of(&tax_hook::ID));
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    let ix = swapped(s.plan_ix(p), pd, programdata_address(&tax_hook::ID));
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    // With both, it plans and pays; the same omissions stop a payment.
    s.plan(p).ok();
    let ix = without(s.pay_ix(p, &[a.pubkey()]), lock);
    refused(&s.send_ixs(&[ix]), CompanionError::StrategyNotAccepted);
    s.pay(p, &[a.pubkey()]).ok();
    // The class functions on a system account at a timelocked program's timelock address.
    let mut lamports = 5_000_000u64;
    let mut data: Vec<u8> = vec![];
    let sys = Pubkey::default();
    let fake = anchor_lang::prelude::AccountInfo::new(
        &lock, false, false, &mut lamports, &mut data, &sys, false,
    );
    let pd_account = s.w.env.account(&pd).unwrap();
    let mut pd_lamports = pd_account.lamports;
    let mut pd_data = pd_account.data.clone();
    let loader_id = pd_account.owner;
    let pd_info = anchor_lang::prelude::AccountInfo::new(
        &pd, false, false, &mut pd_lamports, &mut pd_data, &loader_id, false,
    );
    assert!(classify_programdata(&STRATEGY, &pd_info, Some(&fake)).is_err());
    assert!(!matches!(
        classify_programdata(&STRATEGY, &pd_info, None),
        Ok(AuthorityClass::Timelocked { .. })
    ));
}

/// Two-phase `pay_strategy`: one owner passed four times in one payment is asked once, paid once
/// (`AlreadyPaid` thrice), with one receipt; the same payment sent twice in one transaction pays
/// nobody the second time.
#[test]
fn c_one_owner_is_paid_once_however_it_is_passed() {
    let mut s = Strat::with_world(strategy_world(Some(STUDIO_KEY)), HOUR, false);
    let a = s.buyer(5 * SOL);
    let b = s.buyer(SOL);
    let p = s.start_round(&[&a, &b]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    s.plan(p).ok();
    let before = s.game().epoch_paid;
    let tx = s.pay(p, &[a.pubkey(), a.pubkey(), a.pubkey(), a.pubkey()]);
    tx.ok();
    let paid: StrategyPaid = tx.event();
    assert_eq!(paid.payments.len(), 1);
    let r = reasons(&tx);
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|(o, why)| *o == a.pubkey() && *why == CandidateReason::AlreadyPaid));
    assert_eq!(s.game().epoch_paid - before, paid.payments[0].amount + paid.payments[0].bounty);
    // [pay(b), pay(b)] in one transaction: the second sees the first's receipt.
    let ix = s.pay_ix(p, &[b.pubkey()]);
    let tx = s.send_ixs(&[ix.clone(), ix]);
    tx.ok();
    let paid = tx.events::<StrategyPaid>();
    assert_eq!(paid.len(), 1);
    assert_eq!(paid[0].payments.len(), 1);
    assert_eq!(reasons(&tx), vec![(b.pubkey(), CandidateReason::AlreadyPaid)]);
}
