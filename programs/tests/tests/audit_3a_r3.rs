//! Phase 3a audit, round 3 (confirmation of the round-2 fixes).
//! Report: `bordrless-games-work/log-3a-audit-r3.md`.
//!
//! Each finding has a PoC named `r3_f<N>_...` that asserts the weakness as it is today (a fix flips
//! it); each `c_...` test is a control: an attack tried against a round-2 fix and refused.
//!
//! The strategy harness is the round-2 one (`audit_3a_r2.rs`), trimmed to what these tests use.

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

// =============================================================================== R3-F1

/// R3-F1 (new with the R2-F2 fix). `plan_period` reads the strategy's class from the accounts the
/// sender passes, feeds it to `read_strategy_terms` (an unreadable class counts the audit as void:
/// the 10 SOL cap), and applies those terms (`enforce_terms`: the pot above the cap goes to the
/// buyback) **before** it refuses a missing class (`StrategyNotAccepted`). Three paths return `Ok`
/// in between: a period with no tickets, a forgotten round, and a lapsed (`Late`) period. Anyone who
/// sends `plan_period` on such a period without the strategy's ProgramData therefore trims an
/// audited, Bordrless-managed strategy's uncapped pot to 10 SOL for good: the rest goes to the
/// buyback. With the ProgramData, the same step leaves the pot whole (control).
#[test]
fn r3_f1_leaving_out_the_programdata_trims_an_audited_strategys_pot_on_a_no_ticket_period() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    let set: StrategySet = s.created.event();
    assert_eq!((set.audited, set.pot_cap), (true, 0), "an uncapped game");
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let pot = s.companion().pending_pot;
    let buyback = s.companion().pending_buyback;
    // Control: the round after, nobody entered: an honest plan (class accounts passed) rolls the
    // period over and leaves the pot whole (uncapped).
    let p = s.round();
    s.warp_into(p + 1, 10);
    let tx = s.send(s.plan_ix(p));
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::NoTickets);
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert_eq!(s.companion().pending_pot, pot);
    // The attack: the next empty period, planned by anyone without the strategy's ProgramData.
    // Fixed (round 3): a missing class account is refused before any terms apply.
    s.warp_into(p + 2, 10);
    let ix = without(s.plan_ix(p + 1), programdata_address(&STRATEGY));
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    let c = s.companion();
    assert_eq!(c.pending_pot, pot);
    assert_eq!(c.pending_buyback, buyback);
    // The status is untouched: the strategy is still audited and its class still managed.
    let st: HookStatus = s.w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(st.audited);
    assert_eq!(upgrade_authority(&s.w.env, &STRATEGY), Some(STUDIO_KEY));
}

/// R3-F1, the `Late` path: a period with tickets that nobody planned in time (the keeper paused, a
/// refusing strategy) lapses through the same early return, and the same omission trims the pot.
/// While the period can still be planned, the omission is refused (`StrategyNotAccepted`) and the
/// whole step, trim included, is reverted (control).
#[test]
fn r3_f1_leaving_out_the_programdata_trims_an_audited_strategys_pot_on_a_lapsed_period() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let a = s.buyer(5 * SOL);
    let p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    let pot = s.companion().pending_pot;
    // Control: in time, the omission fails and nothing moves.
    s.warp_into(p + 1, 10);
    let ix = without(s.plan_ix(p), programdata_address(&STRATEGY));
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    assert_eq!(s.companion().pending_pot, pot);
    // Past the plan's deadline (its payments' end less a window): fixed (round 3), still refused;
    // with the accounts, the period lapses and the pot stays whole.
    s.warp_into(p + 1, s.r() - i64::from(WINDOW) + 10);
    let ix = without(s.plan_ix(p), programdata_address(&STRATEGY));
    refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    let tx = s.send(s.plan_ix(p));
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Late);
    assert!(tx.events::<PotToBuyback>().is_empty());
    assert_eq!(s.companion().pending_pot, pot);
}

// =============================================================================== R3-F2

fn raw(env: &bordrless_program_tests::env::Env, key: &Pubkey) -> Option<RawAccount> {
    env.account(key).map(|a| RawAccount {
        owner: a.owner,
        data: a.data,
        executable: a.executable,
    })
}

/// R3-F2 (R2-F2 fix partial). For a Bordrless-managed strategy the companion counts an audit
/// whatever code now runs: `read_strategy_terms` checks the class and that a hash was recorded,
/// never that the code is still the recorded hash (nor the ProgramData slot). Studio's upgrade key
/// (the key of every Studio-managed hook) replaces an audited strategy's code at once, and its pot
/// stays uncapped: a plan leaves 12+ SOL in place, and a new game on the replaced code is made
/// uncapped (`StrategySet { audited: true, pot_cap: 0 }`). The SDK's label, held to the Rust
/// reference, calls the same audit `stale`. Only a later `set_hook_status_v2` un-audit (which does
/// compare the hash) caps it again.
#[test]
fn r3_f2_a_managed_strategys_audit_still_lifts_its_cap_after_its_code_changes() {
    let mut s = Strat::with_world(audited_world(), HOUR);
    let audited_hash = programdata_hash(&s.w.env, &STRATEGY);
    s.grow_pot_above(DEFAULT_POT_CAP + 2 * SOL);
    let pot = s.companion().pending_pot;
    // Studio's key replaces the code (any code: here the pro-rata starter's build).
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
    s.w.env.svm.expire_blockhash();
    send_as(
        &mut s.w.env,
        &[loader::upgrade(pd, STRATEGY, buffer, payer, STUDIO_KEY)],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    s.w.env.warp(1);
    let now_hash = programdata_hash(&s.w.env, &STRATEGY);
    assert_ne!(now_hash, audited_hash);
    assert_eq!(upgrade_authority(&s.w.env, &STRATEGY), Some(STUDIO_KEY));
    // The label (Rust reference of the SDK's): the audit is stale.
    let lock = timelock_of(&STRATEGY);
    let accounts = RiskAccounts {
        program_id: STRATEGY,
        program: raw(&s.w.env, &STRATEGY),
        programdata: raw(&s.w.env, &pd),
        timelock: raw(&s.w.env, &lock),
        timelock_programdata: raw(&s.w.env, &programdata_address(&hook_timelock::ID)),
        status: raw(&s.w.env, &companion::hook_status_address(&STRATEGY)),
        attestation: None,
    };
    let label = risk_label_of(&accounts, s.w.env.now, Some(now_hash));
    assert_eq!((label.class, label.audited), ("managed", "stale"));
    // Fixed (round 3): a plan checks the audit against the code (the deploy slot moved, so the hash
    // is recomputed): it no longer holds, and the pot is capped.
    let p = s.round();
    s.warp_into(p + 1, 10);
    let tx = s.send(s.plan_ix(p));
    tx.ok();
    assert!(pot > DEFAULT_POT_CAP);
    assert_eq!(tx.event::<PotToBuyback>().lamports, pot - DEFAULT_POT_CAP);
    assert_eq!(s.companion().pending_pot, DEFAULT_POT_CAP);
    // A new game on the replaced code is made uncapped.
    let mut w2 = audited_world();
    {
        let code = s.w.env.account(&pd).unwrap();
        w2.env.put(pd, code);
    }
    assert_eq!(programdata_hash(&w2.env, &STRATEGY), now_hash);
    let s2 = Strat::with_world(w2, HOUR);
    let set: StrategySet = s2.created.event();
    assert_eq!((set.audited, set.pot_cap), (false, DEFAULT_POT_CAP));
    // The remedy exists but is manual: the protocol's v2 un-audit (the hash differs) caps it.
    let deployer = s.w.env.deployer.insecure_clone();
    s.w.env.svm.expire_blockhash();
    s.w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                STRATEGY,
                hook_status_args(false, DEFAULT_POT_CAP, false),
                [0; 32],
            )],
            &[&deployer],
        )
        .ok();
}

// =============================================================================== controls

/// Control (R2-F2 fix): the class can't be faked upward. A strategy handed to its author's
/// timelock, audited before the handover, reads as timelocked (capped) whatever the sender passes:
/// with both class accounts it is capped; leaving the Timelock out or the ProgramData out is
/// refused while the period can be planned; a payment without them is refused.
#[test]
fn c_a_timelocked_strategy_cant_be_read_as_managed() {
    let mut w = audited_world();
    let author = w.env.funded(10 * SOL);
    let payer = w.env.payer.pubkey();
    w.env.without_sigverify();
    send_as(
        &mut w.env,
        &[hook_timelock::client::register(
            payer,
            STUDIO_KEY,
            STRATEGY,
            bordrless_hook::authority::MIN_ACCEPTED_DELAY,
            author.pubkey(),
        )],
        &[],
        &[STUDIO_KEY],
    )
    .ok();
    let mut s = Strat::with_world_lock(w, HOUR, true);
    let set: StrategySet = s.created.event();
    assert!(!set.audited);
    assert_eq!(set.pot_cap, DEFAULT_POT_CAP);
    s.grow_pot_above(MIN_POT);
    let a = s.buyer(5 * SOL);
    let p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    s.warp_into(p + 1, 10);
    for key in [programdata_address(&STRATEGY), timelock_of(&STRATEGY)] {
        let ix = without(s.plan_ix(p), key);
        refused(&s.send(ix), CompanionError::StrategyNotAccepted);
    }
    s.send(s.plan_ix(p)).ok();
    for key in [programdata_address(&STRATEGY), timelock_of(&STRATEGY)] {
        let ix = without(s.pay_ix(p, &[a.pubkey()]), key);
        refused(&s.send_ixs(&[ix]), CompanionError::StrategyNotAccepted);
    }
    s.send_ixs(&[s.pay_ix(p, &[a.pubkey()])]).ok();
}

/// Control (R2-F2 fix, un-audit): `set_hook_status_v2` can't lift a live audit. For the audited,
/// still-managed strategy whose code is the recorded hash, an un-audit fails (`BadHookStatus`)
/// with its accounts, and without them (`ProgramAccounts`); a v1 un-audit fails too.
#[test]
fn c_a_live_audit_cant_be_lifted() {
    let mut w = audited_world();
    let deployer = w.env.deployer.insecure_clone();
    let tx = w.env.send(
        &[companion::set_hook_status_v2(
            deployer.pubkey(),
            STRATEGY,
            hook_status_args(false, DEFAULT_POT_CAP, true),
            [0; 32],
        )],
        &[&deployer],
    );
    refused(&tx, CompanionError::BadHookStatus);
    let mut ix = companion::set_hook_status_v2(
        deployer.pubkey(),
        STRATEGY,
        hook_status_args(false, DEFAULT_POT_CAP, false),
        [0; 32],
    );
    ix.accounts.truncate(6);
    w.env.svm.expire_blockhash();
    refused(
        &w.env.send(&[ix], &[&deployer]),
        CompanionError::ProgramAccounts,
    );
    w.env.svm.expire_blockhash();
    let tx = w.env.send(
        &[companion::set_hook_status(
            deployer.pubkey(),
            STRATEGY,
            hook_status_args(false, DEFAULT_POT_CAP, false),
        )],
        &[&deployer],
    );
    refused(&tx, CompanionError::BadHookStatus);
    let st: HookStatus = w.env.read(&companion::hook_status_address(&STRATEGY));
    assert!(st.audited && !st.blocked);
}

/// Control (R2-F1 fix): an active strategy is never retired, even with a period open. Paying
/// a real share every period keeps `settled_at` moving, so `retire` stays `NotDue` long past
/// 60 days from launch (the open period is never closed under it).
#[test]
fn c_a_strategy_that_pays_is_not_retired_with_its_period_open() {
    let mut s = Strat::with_world(strategy_world(Some(STUDIO_KEY)), 7 * 86_400);
    let launched = s.companion().launched_at;
    let a = s.buyer(SOL);
    let mut p = s.round() + 1;
    s.warp_into(p, 5);
    s.enter(&[&a]);
    s.grow_pot_above(MIN_POT);
    while s.w.env.now < launched + 3 * DORMANT_SECS {
        s.warp_into(p + 1, 5);
        s.enter(&[&a]);
        s.send(s.plan_ix(p)).ok();
        assert_eq!(s.game().status, DrawStatus::Revealed);
        s.send_ixs(&[s.pay_ix(p, &[a.pubkey()])]).ok();
        // Its period still open: refused (`DrawPending`, as it is not past `retirable_at`).
        assert!(s.w.env.now < s.game().retirable_at(launched));
        refused(&s.retire(), CompanionError::DrawPending);
        assert!(s.companion().pending_pot > 0);
        p += 1;
    }
    assert!(s.game().settled_at > launched + 2 * DORMANT_SECS);
}
