//! Phase 3a: strategy games (`docs/phase3a.md` §4, §13). A coin launches through its companion with
//! Bordrless's `lottery_hook` for tickets and a builder's strategy deciding each period's budget and
//! each holder's amount; the companion keeps the pot, bounds every answer and pays.
//!
//! The strategy is `fixtures/strategies/tester`, a native program whose answers its config account
//! (an extra it owns, written here directly) scripts, so every row of §4.4 can be driven: a good
//! pro-rata answer, an error, a loop, short, long or no return data, an answer over its bounds, too
//! much compute, a write to a read-only account, a call into the companion, recursion.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{
    CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs, StrategyArgs,
};
use bordrless_companion::state::{
    Companion, DrawStatus, Game, GameKind, ShareReceipt, Split, StrategyTerms,
};
use bordrless_core::policy;
use bordrless_game::{round_of, GameHeader, Slots};
use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
use bordrless_launch::client as launch;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{compute_unit_limit, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::{register, timelock_of};
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
const PACKET: usize = 1_232;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
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

/// The tester's config for `mint`: `PDA(["config", mint], tester)`.
fn config_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config", mint.as_ref()], &STRATEGY).0
}

/// A second extra of the tester (to measure transactions with 2): `PDA(["aux", mint], tester)`.
fn aux_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"aux", mint.as_ref()], &STRATEGY).0
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

/// The tester's config and registry for `mint` (the config, and the aux account when `two`).
fn prepare_strategy(w: &mut World, mint: &Pubkey, two: bool) {
    put_owned(
        w,
        config_address(mint),
        STRATEGY,
        st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_PRO_RATA, 0),
    );
    let mut list = vec![pda_seeds(b"config")];
    if two {
        put_owned(w, aux_address(mint), STRATEGY, vec![1; 8]);
        list.push(pda_seeds(b"aux"));
    }
    put_owned(
        w,
        bordrless_strategy::registry_address(&STRATEGY, mint).0,
        STRATEGY,
        HookAccountList::new(list).encode(),
    );
}

/// A world with the tester loaded, upgradeable by `authority` (Studio's key: managed).
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

fn extras(mint: &Pubkey, two: bool) -> Vec<Pubkey> {
    let mut e = vec![config_address(mint)];
    if two {
        e.push(aux_address(mint));
    }
    e
}

fn setup_ixs(
    launcher: &Pubkey,
    mint: &Pubkey,
    g: CreateGameArgs,
    s: StrategyArgs,
    two: bool,
) -> Vec<Instruction> {
    vec![
        companion::create(*launcher, *launcher, *mint, create_args()),
        lottery::prepare(*launcher, *mint, g.round_secs),
        companion::create_strategy_game(*launcher, *mint, g, s, vec![], false, &extras(mint, two)),
    ]
}

/// The protocol's 22-address lookup table, as the SDK lists it.
fn table_22(w: &World) -> Vec<Pubkey> {
    let mut addresses = protocol_lookup_table(w);
    addresses.extend([
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    assert_eq!(addresses.len(), 22);
    addresses
}

/// A strategy coin launched through its companion, past the sniper window.
struct Strat {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
    two: bool,
}

impl Strat {
    fn new() -> Self {
        Self::with(Some(STUDIO_KEY), |_, _| {}, false)
    }

    fn with(
        authority: Option<Pubkey>,
        f: impl FnOnce(&mut CreateGameArgs, &mut StrategyArgs),
        two: bool,
    ) -> Self {
        let mut w = world(authority);
        let launcher = w.wallet_with_sol(50 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(&mut w, &mint, two);
        let (mut g, mut s) = (game_args(), strategy_args());
        f(&mut g, &mut s);
        let tx = w.env.send_paid_by(
            &setup_ixs(&launcher.pubkey(), &mint, g, s, two),
            &launcher,
            &[&mint_kp],
        );
        tx.ok();
        let set: StrategySet = tx.event();
        assert_eq!(set.strategy, STRATEGY);
        let config = hook_config(&mut w, &launcher);
        let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config);
        w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
        w.env.warp(31);
        let cranker = w.wallet_with_sol(5 * SOL);
        Self {
            w,
            mint,
            cranker,
            two,
        }
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

    fn terms(&self) -> StrategyTerms {
        self.w
            .env
            .read(&companion::strategy_terms_address(&self.mint))
    }

    fn header(&self) -> GameHeader {
        let data = self
            .w
            .env
            .account(&lottery::state_address(&self.mint))
            .unwrap()
            .data;
        GameHeader::read(&data, &self.mint).unwrap()
    }

    fn balance(&self, owner: &Pubkey) -> u64 {
        self.w.env.holding(&self.mint, owner)
    }

    fn weight(&self, owner: &Pubkey, round: u32) -> u64 {
        Slots::decode(&self.w.env.hook_data(&self.mint, owner))
            .range_in(round)
            .map_or(0, |r| r.weight)
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

    /// The next round, 5 seconds in, with `holders` entered.
    fn start_round(&mut self, holders: &[&Keypair]) -> u32 {
        let next = self.round() + 1;
        self.warp_into(next, 5);
        self.enter(holders);
        next
    }

    fn claim_fees(&mut self) -> Tx {
        let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
        self.send(ix)
    }

    fn fund_pot(&mut self) {
        self.volume(1, 20 * SOL);
        self.claim_fees().ok();
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
            &extras(&self.mint, self.two),
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
            &extras(&self.mint, self.two),
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

    fn set_status(&mut self, program: Pubkey, audited: bool, pot_cap: u64, blocked: bool) -> Tx {
        let deployer = self.w.env.deployer.insecure_clone();
        let ix = companion::set_hook_status(
            deployer.pubkey(),
            program,
            HookStatusArgs {
                audited,
                pot_cap,
                blocked,
            },
        );
        self.w.env.send_paid_by(&[ix], &deployer, &[])
    }

    /// Holders A (5 SOL of tokens), B (1 SOL) and C (2 SOL) entered in a fresh period, a funded pot,
    /// and that period over: answers the holders and the period.
    fn period_with_holders(&mut self) -> (Keypair, Keypair, Keypair, u32) {
        let a = self.buyer(5 * SOL);
        let b = self.buyer(SOL);
        let c = self.buyer(2 * SOL);
        let p = self.start_round(&[&a, &b, &c]);
        self.fund_pot();
        self.warp_into(p + 1, 10);
        (a, b, c, p)
    }
}

/// The heap a transaction's companion instruction peaked at, when the companion was built with the
/// instrumented allocator (`custom-heap` and the heap probe, which logs `0x4ea9` and the bytes used at
/// each new 256-byte high-water mark); `None` with the deployed build.
fn heap_peak(tx: &Tx) -> Option<u64> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program log: 0x4ea9, 0x"))
        .filter_map(|rest| u64::from_str_radix(rest.split(',').next()?, 16).ok())
        .max()
}

fn reasons(tx: &Tx) -> Vec<(Pubkey, CandidateReason)> {
    tx.events::<CandidateRejected>()
        .into_iter()
        .map(|e| (e.owner, e.reason))
        .collect()
}

// =============================================================================== creation

#[test]
fn a_strategy_game_is_made_with_its_terms() {
    let s = Strat::new();
    let g = s.game();
    assert_eq!(
        (g.kind, g.hook, g.min_weight, g.prize_bps, g.max_attempts),
        (GameKind::Strategy, HOOK, 1, 0, 0)
    );
    let t = s.terms();
    assert_eq!(
        (t.strategy, t.extras(), t.budget_bps, t.max_share_bps, t.max_per_tx),
        (
            STRATEGY,
            &[config_address(&s.mint)][..],
            5_000,
            2_500,
            4
        )
    );
    assert_eq!(s.companion().game_kind, GameKind::Strategy);
    assert_eq!(StrategyTerms::LEN, t_len());
    // The launch took the lottery hook with its flags.
    assert_eq!(s.w.launch(&s.mint).custom_hook, Some(HOOK));
}

fn t_len() -> usize {
    8 + 1 + 1 + 32 + 32 + 32 + 1 + 64 + 1 + 2 + 2 + 1 + 4 + 4 + 4 + 8 + 8 + 32
}

#[test]
fn creation_bounds_are_checked() {
    type Tweak = fn(&mut CreateGameArgs, &mut StrategyArgs);
    let bad_strategy: [Tweak; 11] = [
        |_, s| s.budget_bps = 0,
        |_, s| s.budget_bps = MAX_STRATEGY_BUDGET_BPS + 1,
        |_, s| s.max_share_bps = 0,
        |_, s| s.max_share_bps = MAX_STRATEGY_SHARE_BPS + 1,
        |_, s| s.max_per_tx = 0,
        |_, s| s.max_per_tx = MAX_STRATEGY_PER_TX + 1,
        |_, s| s.plan_cu_max = 0,
        |_, s| s.plan_cu_max = MAX_PLAN_CU + 1,
        |_, s| s.entitle_cu_max = 0,
        |_, s| s.entitle_cu_max = MAX_ENTITLE_CU + 1,
        |_, s| s.min_weight = 0,
    ];
    let bad_game: [Tweak; 7] = [
        |g, _| g.prize_bps = 1_000,
        |g, _| g.max_attempts = 1,
        |g, _| g.round_secs = 3_599,
        |g, _| g.claim_window_secs = 299,
        |g, _| g.claim_window_secs = 1_801,
        |g, _| g.min_pot = MIN_MIN_POT - 1,
        |g, _| g.kind = GameKind::Streak,
    ];
    let mut w = world(Some(STUDIO_KEY));
    let mut try_with = |f: Tweak| {
        let launcher = w.wallet_with_sol(5 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(&mut w, &mint, false);
        let (mut g, mut s) = (game_args(), strategy_args());
        f(&mut g, &mut s);
        w.env.send_paid_by(
            &setup_ixs(&launcher.pubkey(), &mint, g, s, false),
            &launcher,
            &[&mint_kp],
        )
    };
    for f in bad_strategy {
        refused(&try_with(f), CompanionError::BadStrategy);
    }
    for (i, f) in bad_game.into_iter().enumerate() {
        let tx = try_with(f);
        if i == 6 {
            refused(&tx, CompanionError::WrongGameKind);
        } else if i == 2 {
            // The hook's header says 3,600.
            tx.expect_fail();
        } else {
            refused(&tx, CompanionError::BadGame);
        }
    }
    // create_game_v2 refuses the kind: a strategy game has terms of its own.
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        companion::create_game_v2(launcher.pubkey(), mint, game_args(), GameKindArgs::default()),
    ];
    refused(
        &w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]),
        CompanionError::WrongGameKind,
    );
}

#[test]
fn only_a_bounded_strategy_is_accepted() {
    let make = |w: &mut World, s: StrategyArgs, timelocked: bool| -> Tx {
        let launcher = w.wallet_with_sol(5 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(w, &mint, false);
        let ixs = [
            companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
            lottery::prepare(launcher.pubkey(), mint, ROUND),
            companion::create_strategy_game(
                launcher.pubkey(),
                mint,
                game_args(),
                s,
                vec![],
                timelocked,
                &extras(&mint, false),
            ),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
    };
    // Immutable, Studio's key, the protocol's key: taken.
    for authority in [
        None,
        Some(STUDIO_KEY),
        Some(bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1]),
    ] {
        let mut w = world(authority);
        let tx = make(&mut w, strategy_args(), false);
        tx.ok();
        let set: StrategySet = tx.event();
        let class = match authority {
            None => strategy_class::IMMUTABLE,
            _ => strategy_class::MANAGED,
        };
        assert_eq!((set.class, set.audited, set.pot_cap), (class, false, DEFAULT_POT_CAP));
    }
    // An author's key: refused, even with a status from the protocol (round 1, M-6/M-7: a status
    // sets terms, never lets code its author can change at will in).
    let author = Keypair::new();
    let mut w = world(Some(author.pubkey()));
    refused(
        &make(&mut w, strategy_args(), false),
        CompanionError::StrategyNotAccepted,
    );
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send_paid_by(
            &[companion::set_hook_status(
                deployer.pubkey(),
                STRATEGY,
                HookStatusArgs {
                    audited: false,
                    pot_cap: SOL,
                    blocked: false,
                },
            )],
            &deployer,
            &[],
        )
        .ok();
    refused(
        &make(&mut w, strategy_args(), false),
        CompanionError::StrategyNotAccepted,
    );
    // Bordrless-managed with a status: taken, on the status's terms; blocked: refused.
    let mut w = world(Some(STUDIO_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let status = |blocked| {
        companion::set_hook_status(
            deployer.pubkey(),
            STRATEGY,
            HookStatusArgs {
                audited: false,
                pot_cap: SOL,
                blocked,
            },
        )
    };
    w.env.send_paid_by(&[status(false)], &deployer, &[]).ok();
    let tx = make(&mut w, strategy_args(), false);
    tx.ok();
    let set: StrategySet = tx.event();
    assert_eq!((set.class, set.pot_cap), (strategy_class::STATUS, SOL));
    w.env.send_paid_by(&[status(true)], &deployer, &[]).ok();
    refused(
        &make(&mut w, strategy_args(), false),
        CompanionError::StrategyNotAccepted,
    );
    // Timelocked: taken with its Timelock (no attestation needed for a strategy).
    let author = Keypair::new();
    let mut w = world(Some(author.pubkey()));
    w.env.fund(author.pubkey(), 10 * SOL);
    register(&mut w.env, &author, STRATEGY, 3 * 86_400, author.pubkey()).ok();
    refused(
        &make(&mut w, strategy_args(), false),
        CompanionError::StrategyNotAccepted,
    );
    let tx = make(&mut w, strategy_args(), true);
    tx.ok();
    assert_eq!(tx.event::<StrategySet>().class, strategy_class::TIMELOCKED);
    assert_eq!(
        bordrless_program_tests::timelock::upgrade_authority(&w.env, &STRATEGY),
        Some(timelock_of(&STRATEGY))
    );
    // The protocol's own programs, the game hook, a non-program: refused.
    let mut w = world(Some(STUDIO_KEY));
    for bad in [HOOK, bordrless_token::ID, bordrless_companion::ID] {
        let s = StrategyArgs {
            strategy: bad,
            ..strategy_args()
        };
        refused(&make(&mut w, s, false), CompanionError::StrategyNotAccepted);
    }
    let wallet = w.wallet_with_sol(SOL);
    let s = StrategyArgs {
        strategy: wallet.pubkey(),
        ..strategy_args()
    };
    refused(&make(&mut w, s, false), CompanionError::StrategyNotAccepted);
}

#[test]
fn a_registry_lists_only_the_strategys_own_accounts() {
    let make = |w: &mut World, list: Vec<ExtraAccount>, pass: Vec<Pubkey>| -> Tx {
        let launcher = w.wallet_with_sol(5 * SOL);
        let mint_kp = Keypair::new();
        let mint = mint_kp.pubkey();
        prepare_strategy(w, &mint, true);
        put_owned(
            w,
            bordrless_strategy::registry_address(&STRATEGY, &mint).0,
            STRATEGY,
            HookAccountList::new(list).encode(),
        );
        let mut pass = pass;
        pass.extend([config_address(&mint), aux_address(&mint)]);
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
                &pass,
            ),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
    };
    let mut w = world(Some(STUDIO_KEY));
    let sysvar = Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111");
    let key = |k: Pubkey| ExtraAccount {
        writable: false,
        source: AccountSource::Key(k),
    };
    // The instructions sysvar (which would show the whole transaction), the clock: refused.
    refused(
        &make(&mut w, vec![key(sysvar)], vec![sysvar]),
        CompanionError::StrategyRegistry,
    );
    let clock = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    refused(
        &make(&mut w, vec![key(clock)], vec![clock]),
        CompanionError::StrategyRegistry,
    );
    // Three accounts: refused.
    refused(
        &make(
            &mut w,
            vec![pda_seeds(b"config"), pda_seeds(b"aux"), pda_seeds(b"config")],
            vec![],
        ),
        CompanionError::StrategyRegistry,
    );
    // A seed the strategy can't name (the source owner, an index beyond [mint, game]).
    let bad_seed = ExtraAccount {
        writable: false,
        source: AccountSource::Pda {
            program: STRATEGY,
            seeds: vec![Seed::Literal(b"config".to_vec()), Seed::Account(2)],
        },
    };
    refused(
        &make(&mut w, vec![bad_seed], vec![]),
        CompanionError::StrategyRegistry,
    );
    // A listed account not passed.
    let missing = Keypair::new().pubkey();
    refused(
        &make(&mut w, vec![key(missing)], vec![]),
        CompanionError::StrategyRegistry,
    );
    // Two of its own: taken.
    make(&mut w, vec![pda_seeds(b"config"), pda_seeds(b"aux")], vec![]).ok();
    // No registry at all: no extras (the tester then can't read its config, which is its own
    // problem: the plan fails and the transaction with it).
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
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
            &[],
        ),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
    let t: StrategyTerms = w.env.read(&companion::strategy_terms_address(&mint));
    assert_eq!(t.n_extras, 0);
}

// =============================================================================== the happy path

#[test]
fn a_strategy_pays_pro_rata_over_three_periods() {
    let mut s = Strat::new();
    let (a, b, c, p) = s.period_with_holders();
    let total = s.header().total_of(p).unwrap();
    let (wa, wb, wc) = (
        s.weight(&a.pubkey(), p),
        s.weight(&b.pubkey(), p),
        s.weight(&c.pubkey(), p),
    );
    assert_eq!(wa + wb + wc, total);
    let pot = s.companion().pending_pot;
    // Plan: the tester answers all it may, half the pot.
    let tx = s.plan(p);
    tx.ok();
    let ev: PeriodPlanned = tx.event();
    assert_eq!(ev.budget, pot / 2);
    assert_eq!((ev.period, ev.total), (p, total));
    println!("plan_period: {} CU, {} bytes", tx.cu(), tx.size);
    let g = s.game();
    assert_eq!((g.status, g.round, g.prize), (DrawStatus::Revealed, p, pot / 2));
    assert_eq!(s.companion().pot_locked, pot / 2);
    // Twice: refused (the period is planned).
    s.plan(p).expect_fail();
    // Pay A, B and C in one transaction.
    let budget = pot / 2;
    let before: Vec<u64> = [&a, &b, &c]
        .iter()
        .map(|k| s.w.env.lamports(&k.pubkey()))
        .collect();
    let cranker_before = s.w.env.lamports(&s.cranker.pubkey());
    let tx = s.pay(p, &[a.pubkey(), b.pubkey(), c.pubkey()]);
    tx.ok();
    println!("pay_strategy x3: {} CU, {} bytes", tx.cu(), tx.size);
    let paid: StrategyPaid = tx.event();
    assert_eq!(paid.payments.len(), 3);
    let mut sum = 0;
    let cap = budget * u64::from(MAX_STRATEGY_SHARE_BPS) / 10_000;
    for (i, (k, wt)) in [(&a, wa), (&b, wb), (&c, wc)].into_iter().enumerate() {
        // Pro rata, at most what the companion lets one holder take (the tester keeps within it,
        // as a well-written strategy does).
        let amount = ((u128::from(budget) * u128::from(wt) / u128::from(total)) as u64)
            .min(cap)
            .min(budget - sum);
        let bounty = amount * u64::from(BOUNTY_BPS) / 10_000;
        assert_eq!(paid.payments[i].owner, k.pubkey());
        assert_eq!(paid.payments[i].amount, amount - bounty);
        assert_eq!(s.w.env.lamports(&k.pubkey()), before[i] + amount - bounty);
        let r: ShareReceipt = s.w.env.read(&companion::receipt_address(&s.mint, p, &k.pubkey()));
        assert_eq!((r.epoch, r.owner, r.amount), (p, k.pubkey(), amount - bounty));
        sum += amount;
    }
    assert_eq!(s.game().epoch_paid, sum);
    assert_eq!(s.companion().pot_locked, budget - sum);
    assert_eq!(s.companion().pending_pot, pot - sum);
    // The sender paid the receipts' rent and got the bounties.
    let rent = s.w.env.rent(ShareReceipt::LEN);
    assert_eq!(
        s.w.env.lamports(&s.cranker.pubkey()) + 3 * rent,
        cranker_before + paid.bounty - 5_000
    );
    // Paid once: a second payment of A is refused (its receipt), nothing moves.
    let tx = s.pay(p, &[a.pubkey()]);
    tx.ok();
    assert_eq!(reasons(&tx), vec![(a.pubkey(), CandidateReason::AlreadyPaid)]);
    assert!(tx.events::<StrategyPaid>().is_empty());

    // Period 2: the unpaid rounding rolls over; the next plan releases it.
    s.enter(&[&a, &b, &c]);
    s.fund_pot();
    s.warp_into(p + 2, 10);
    let tx = s.plan(p + 1);
    tx.ok();
    let ended: EpochEnded = tx.event();
    assert_eq!((ended.epoch, ended.unclaimed), (p, budget - sum));
    s.pay(p + 1, &[a.pubkey(), b.pubkey()]).ok();
    // Period 3, with a budget of 10% (the tester's param).
    s.set_config((st::MODE_PRO_RATA, 1_000), (st::MODE_PRO_RATA, 0));
    s.enter(&[&a, &b, &c]);
    s.warp_into(p + 3, 10);
    let pot = s.companion().pending_pot;
    let tx = s.plan(p + 2);
    tx.ok();
    assert_eq!(tx.event::<PeriodPlanned>().budget, pot / 2 / 10);
    s.pay(p + 2, &[c.pubkey()]).ok();
    // Receipts close after their period's payments end, the rent to the sender.
    let rk = companion::receipt_address(&s.mint, p, &a.pubkey());
    let before = s.w.env.lamports(&s.cranker.pubkey());
    let ix = companion::close_receipt(s.mint, p, a.pubkey(), s.cranker.pubkey());
    s.send(ix).ok();
    assert!(s.w.env.account(&rk).is_none_or(|x| x.lamports == 0));
    assert!(s.w.env.lamports(&s.cranker.pubkey()) > before);
    let t = s.terms();
    assert_eq!(t.periods_planned, 3);
    assert!(t.paid_total > 0);
}

// =============================================================================== §4.4: misbehaving strategies

#[test]
fn a_plan_answer_that_is_refused_leaves_the_period_unplanned() {
    for (mode, param, want) in [
        (st::MODE_SHORT, 0, AnswerFault::BadLength),
        (st::MODE_LONG, 0, AnswerFault::BadLength),
        (st::MODE_NONE, 0, AnswerFault::NoAnswer),
        (st::MODE_MAX, 0, AnswerFault::OverBound),
    ] {
        let mut s = Strat::new();
        let (a, _, _, p) = s.period_with_holders();
        s.set_config((mode, param), (st::MODE_PRO_RATA, 0));
        let pot = s.companion().pending_pot;
        let tx = s.plan(p);
        tx.ok();
        let ev: PeriodRejected = tx.event();
        assert_eq!((ev.period, ev.reason), (p, want), "mode {mode}");
        let g = s.game();
        assert_eq!(g.status, DrawStatus::Idle);
        assert!(g.next_round <= p, "mode {mode}: a refused answer uses up no period");
        assert_eq!(s.companion().pending_pot, pot);
        assert_eq!(s.companion().pot_locked, 0);
        refused(&s.pay(p, &[a.pubkey()]), CompanionError::PeriodNotOpen);
        // Asked again (round 1, M-4: nobody voids a period by asking at a chosen moment), a valid
        // answer plans it.
        s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_PRO_RATA, 0));
        s.w.env.svm.expire_blockhash();
        s.plan(p).ok();
        assert_eq!(s.game().next_round, p + 1);
        s.pay(p, &[a.pubkey()]).ok();
    }
}

#[test]
fn a_strategy_over_the_transactions_compute_fails_it() {
    // The companion can't meter a call (the syscall is inactive on mainnet): a strategy that uses
    // more than the transaction's limit fails it, nothing moves, and the period stays plannable.
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_BURN, 300_000), (st::MODE_PRO_RATA, 0));
    let ix = s.plan_ix(p);
    let cranker = s.cranker.insecure_clone();
    let tx = s
        .w
        .env
        .send_v0(&[compute_unit_limit(200_000), ix.clone()], &cranker, &[], &[]);
    tx.expect_fail();
    assert!(s.game().next_round <= p);
    // With room, the same call plans.
    s.w.env
        .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[])
        .ok();
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_BURN, 500_000));
    let pay = s.pay_ix(p, &[a.pubkey()]);
    s.w.env
        .send_v0(&[compute_unit_limit(300_000), pay], &cranker, &[], &[])
        .expect_fail();
}

#[test]
fn an_owner_twice_in_one_payment_is_paid_once() {
    // Receipts are made after every question (round 1, M-2), so a candidate repeated in one
    // payment is caught by the payment itself, not by its receipt.
    let mut s = Strat::new();
    let (a, b, _, p) = s.period_with_holders();
    s.plan(p).ok();
    let before = s.w.env.lamports(&a.pubkey());
    let tx = s.pay(p, &[a.pubkey(), b.pubkey(), a.pubkey()]);
    tx.ok();
    assert_eq!(reasons(&tx), vec![(a.pubkey(), CandidateReason::AlreadyPaid)]);
    let paid: StrategyPaid = tx.event();
    assert_eq!(paid.payments.len(), 2);
    assert_eq!(paid.payments[0].owner, a.pubkey());
    assert_eq!(s.w.env.lamports(&a.pubkey()), before + paid.payments[0].amount);
    assert_eq!(
        s.game().epoch_paid,
        paid.payments.iter().map(|x| x.amount).sum::<u64>() + paid.bounty
    );
}

#[test]
fn a_plan_of_zero_skips_the_period() {
    let mut s = Strat::new();
    let (_, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_FLAT, 0), (st::MODE_PRO_RATA, 0));
    let tx = s.plan(p);
    tx.ok();
    assert_eq!(tx.event::<PeriodSkipped>().period, p);
    assert_eq!(s.game().status, DrawStatus::Idle);
}

#[test]
fn a_strategy_that_fails_fails_the_transaction_and_moves_nothing() {
    for mode in [
        st::MODE_ERROR,
        st::MODE_LOOP,
        st::MODE_WRITE,
        st::MODE_CALL_COMPANION,
    ] {
        let mut s = Strat::new();
        let (a, _, _, p) = s.period_with_holders();
        s.set_config((mode, 0), (st::MODE_PRO_RATA, 0));
        let pot = s.companion().pending_pot;
        s.plan(p).expect_fail();
        assert_eq!(s.companion().pending_pot, pot);
        assert!(s.game().next_round <= p, "mode {mode}: still plannable");
        // As `entitle`: the payment fails, nothing paid, no receipt.
        s.set_config((st::MODE_PRO_RATA, 0), (mode, 0));
        s.plan(p).ok();
        let before = s.w.env.lamports(&a.pubkey());
        s.pay(p, &[a.pubkey()]).expect_fail();
        assert_eq!(s.w.env.lamports(&a.pubkey()), before);
        assert!(s
            .w
            .env
            .account(&companion::receipt_address(&s.mint, p, &a.pubkey()))
            .is_none());
    }
}

#[test]
fn every_account_reaches_the_strategy_read_only_and_unsigned() {
    let mut s = Strat::new();
    let (a, b, _, p) = s.period_with_holders();
    s.set_config(
        (st::MODE_CHECK_PRIVILEGES, 0),
        (st::MODE_CHECK_PRIVILEGES, 0),
    );
    s.plan(p).ok();
    let tx = s.pay(p, &[a.pubkey(), b.pubkey()]);
    tx.ok();
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 2);
    // No program comes with the accounts, not even the strategy's own: it can call nothing (not
    // itself, not the companion), so no answer can come from a program it called.
    let mut s = Strat::new();
    let (_, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_RECURSE, 0), (st::MODE_RECURSE, 0));
    let tx = s.plan(p);
    tx.expect_fail();
    assert!(tx.logs().iter().any(|l| l.contains("Unknown program")));
}

#[test]
fn entitlements_are_bounded_never_clamped() {
    let mut s = Strat::new();
    let (a, b, c, p) = s.period_with_holders();
    // u64::MAX for everyone: nobody is paid (not the cap to everyone).
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_MAX, 0));
    s.plan(p).ok();
    let tx = s.pay(p, &[a.pubkey(), b.pubkey()]);
    tx.ok();
    assert_eq!(
        reasons(&tx),
        vec![
            (a.pubkey(), CandidateReason::Answer(AnswerFault::OverBound)),
            (b.pubkey(), CandidateReason::Answer(AnswerFault::OverBound)),
        ]
    );
    assert_eq!(s.game().epoch_paid, 0);
    // A flat amount at the per-holder cap (25% of the budget) is paid, one over it is not.
    let budget = s.game().prize;
    let cap = budget * 2_500 / 10_000;
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, cap + 1));
    let tx = s.pay(p, &[a.pubkey()]);
    assert_eq!(
        reasons(&tx),
        vec![(a.pubkey(), CandidateReason::Answer(AnswerFault::OverBound))]
    );
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, cap));
    let tx = s.pay(p, &[a.pubkey(), b.pubkey(), c.pubkey()]);
    tx.ok();
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 3);
    // The budget runs out first come: the fourth holder at the cap finds only a quarter left.
    let d = s.buyer(SOL);
    let _ = d;
    assert_eq!(s.game().epoch_paid, 3 * cap);
    // Short, long, none and too much compute: skipped, the others go on.
    for (mode, param, fault) in [
        (st::MODE_SHORT, 0, AnswerFault::BadLength),
        (st::MODE_NONE, 0, AnswerFault::NoAnswer),
    ] {
        let mut s = Strat::new();
        let (a, _, _, p) = s.period_with_holders();
        s.plan(p).ok();
        s.set_config((st::MODE_PRO_RATA, 0), (mode, param));
        let tx = s.pay(p, &[a.pubkey()]);
        tx.ok();
        assert_eq!(reasons(&tx), vec![(a.pubkey(), CandidateReason::Answer(fault))]);
    }
}

#[test]
fn a_rigged_strategy_pays_its_favourite_at_most_the_cap() {
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FAVOURITE, u64::MAX / 4));
    s.plan(p).ok();
    // The favourite holds nothing: not even a candidate.
    let tx = s.pay(p, &[st::FAVOURITE]);
    tx.ok();
    assert_eq!(reasons(&tx), vec![(st::FAVOURITE, CandidateReason::WrongHolding)]);
    // Others get their pro-rata share.
    let tx = s.pay(p, &[a.pubkey()]);
    tx.ok();
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 1);
}

// =============================================================================== candidates

#[test]
fn every_candidate_exclusion_holds() {
    let mut s = Strat::new();
    let (a, b, _, p) = s.period_with_holders();
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, 1_000_000));
    s.plan(p).ok();
    let mint = s.mint;
    let l = s.w.launch(&mint);
    let excluded = [
        launch::launch_address(&mint),
        l.pool,
        companion::creator_address(&mint),
        companion::companion_address(&mint),
        companion::game_address(&mint),
        companion::strategy_terms_address(&mint),
        companion::oracle_payer_address(&mint),
        STRATEGY,
        HOOK,
    ];
    for owner in excluded {
        let tx = s.pay(p, &[owner]);
        tx.ok();
        let r = reasons(&tx);
        assert_eq!(r.len(), 1, "{owner}");
        assert!(
            matches!(r[0].1, CandidateReason::WrongHolding | CandidateReason::NotEligible),
            "{owner}: {:?}",
            r[0].1
        );
    }
    // The pool and the launch do hold the coin: they are refused for eligibility.
    let tx = s.pay(p, &[l.pool]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::NotEligible);
    // Another mint's holding at the owner's slot: the holding must be this mint's.
    let mut ix = s.pay_ix(p, &[a.pubkey()]);
    let n = ix.accounts.len();
    ix.accounts[n - 3] = AccountMeta::new_readonly(
        bordrless_token::client::holding_address(&s.w.sol, &a.pubkey()),
        false,
    );
    let cranker = s.cranker.insecure_clone();
    let tx = s
        .w
        .env
        .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[]);
    tx.ok();
    assert_eq!(reasons(&tx)[0].1, CandidateReason::WrongHolding);
    // B's holding named for A: refused.
    let mut ix = s.pay_ix(p, &[a.pubkey()]);
    ix.accounts[n - 3] = AccountMeta::new_readonly(
        bordrless_token::client::holding_address(&mint, &b.pubkey()),
        false,
    );
    let tx = s
        .w
        .env
        .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::WrongHolding);
    // A wrong receipt address.
    let mut ix = s.pay_ix(p, &[a.pubkey()]);
    ix.accounts[n - 1] = AccountMeta::new(Keypair::new().pubkey(), false);
    let tx = s
        .w
        .env
        .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::WrongHolding);
    // A holder who bought during the period: no weight in it.
    let late = s.buyer(SOL);
    let tx = s.pay(p, &[late.pubkey()]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::NoWeight);
    // A holder who sold everything since: no holding, or a weight above its balance.
    let held = s.balance(&b.pubkey());
    s.w.sell(&b, &mint, held / 2).ok();
    let tx = s.pay(p, &[b.pubkey()]);
    tx.ok();
    // A send cuts the range to the balance left: still paid, on what it kept.
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 1);
}

#[test]
fn min_weight_and_empty_wallets() {
    let mut s = Strat::with(Some(STUDIO_KEY), |_, st| st.min_weight = 10u64.pow(15), false);
    let (a, _, _, p) = s.period_with_holders();
    s.plan(p).ok();
    let tx = s.pay(p, &[a.pubkey()]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::NoWeight);
    // An empty wallet can't take less than its rent-exempt minimum.
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, 1_000));
    s.plan(p).ok();
    let mut acc = s.w.env.account(&a.pubkey()).unwrap();
    acc.lamports = 0;
    s.w.env.put(a.pubkey(), acc);
    let tx = s.pay(p, &[a.pubkey()]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::BelowRent);
    // Zero: nothing, no receipt.
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_FLAT, 0));
    let tx = s.pay(p, &[a.pubkey()]);
    assert_eq!(reasons(&tx)[0].1, CandidateReason::Zero);
    assert!(s
        .w
        .env
        .account(&companion::receipt_address(&s.mint, p, &a.pubkey()))
        .is_none());
}

#[test]
fn duplicates_in_one_transaction_are_paid_once_and_prefunded_receipts_still_work() {
    let mut s = Strat::new();
    let (a, b, _, p) = s.period_with_holders();
    s.plan(p).ok();
    // Someone funds B's receipt address first: B is still paid.
    s.w.env
        .fund(companion::receipt_address(&s.mint, p, &b.pubkey()), 1_000);
    let tx = s.pay(p, &[a.pubkey(), a.pubkey(), b.pubkey()]);
    tx.ok();
    let paid = tx.event::<StrategyPaid>();
    assert_eq!(
        paid.payments.iter().map(|x| x.owner).collect::<Vec<_>>(),
        vec![a.pubkey(), b.pubkey()]
    );
    assert_eq!(reasons(&tx), vec![(a.pubkey(), CandidateReason::AlreadyPaid)]);
    let r: ShareReceipt = s.w.env.read(&companion::receipt_address(&s.mint, p, &b.pubkey()));
    assert_eq!(r.owner, b.pubkey());
    // More than max_per_tx, or none: refused.
    let five: Vec<Pubkey> = (0..5).map(|_| Keypair::new().pubkey()).collect();
    refused(&s.pay(p, &five), CompanionError::TooManyCandidates);
    refused(&s.pay(p, &[]), CompanionError::TooManyCandidates);
}

// =============================================================================== the pot

#[test]
fn a_block_mid_period_sends_the_locked_budget_to_the_buyback() {
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.plan(p).ok();
    let c = s.companion();
    assert!(c.pot_locked > 0);
    s.set_status(STRATEGY, false, DEFAULT_POT_CAP, true).ok();
    let tx = s.pay(p, &[a.pubkey()]);
    tx.ok();
    let moved: PotToBuyback = tx.event();
    assert!(moved.blocked);
    let c2 = s.companion();
    assert_eq!((c2.pending_pot, c2.pot_locked), (0, 0));
    assert_eq!(c2.pending_buyback, c.pending_buyback + c.pending_pot);
    assert_eq!(s.game().status, DrawStatus::Idle);
    assert!(tx.events::<StrategyPaid>().is_empty());
    // Blocking the ticket hook does the same for a fresh game.
    let mut s = Strat::new();
    let (_, _, _, p) = s.period_with_holders();
    s.plan(p).ok();
    s.set_status(HOOK, false, DEFAULT_POT_CAP, true).ok();
    let ix = s.plan_ix(p + 1);
    let tx = s.send(ix);
    tx.ok();
    assert_eq!(s.companion().pending_pot, 0);
}

#[test]
fn a_cap_lowered_mid_period_keeps_the_locked_budget() {
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.plan(p).ok();
    let locked = s.companion().pot_locked;
    s.set_status(STRATEGY, false, MIN_POT_CAP, false).ok();
    let tx = s.pay(p, &[a.pubkey()]);
    tx.ok();
    let c = s.companion();
    // The pot is trimmed to the cap but never below what the period still owes.
    assert!(c.pending_pot <= MIN_POT_CAP.max(c.pot_locked));
    assert!(c.pot_locked < locked);
    // The next plan's budget is half the trimmed pot.
    s.warp_into(p + 2, 10);
    s.enter(&[&a]);
}

#[test]
fn a_period_lapses_when_not_planned_in_time_and_the_pot_stays() {
    let mut s = Strat::new();
    let (_, _, _, p) = s.period_with_holders();
    // Not before it is over, not another period.
    refused(&s.plan(p + 1), CompanionError::RoundNotOver);
    refused(&s.plan(p - 1), CompanionError::RoundNotOver);
    let pot = s.companion().pending_pot;
    // After its deadline (a claim window before its payments end): Late.
    s.warp_into(p + 2, -(i64::from(WINDOW)) + 1);
    let tx = s.plan(p);
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::Late);
    assert_eq!(s.companion().pending_pot, pot);
    // A period with no tickets lapses too.
    s.warp_into(p + 3, 10);
    let tx = s.plan(p + 2);
    tx.ok();
    assert_eq!(tx.event::<RolledOver>().reason, RolloverReason::NoTickets);
}

#[test]
fn a_strategy_that_never_pays_is_retired_after_two_dormant_periods() {
    let mut s = Strat::new();
    let (_, _, _, p) = s.period_with_holders();
    s.set_config((st::MODE_NONE, 0), (st::MODE_PRO_RATA, 0));
    s.plan(p).ok();
    let retire = |s: &mut Strat| {
        let ix = companion::retire_game(s.cranker.pubkey(), s.mint, HOOK);
        s.send(ix)
    };
    refused(&retire(&mut s), CompanionError::NotDue);
    // 60 days from the launch, no period paid: the pot goes to the buyback, nobody paid.
    s.w.env.warp(2 * DORMANT_SECS);
    let pot = s.companion().pending_pot;
    let tx = retire(&mut s);
    tx.ok();
    assert_eq!(tx.event::<PotRetired>().lamports, pot);
    assert_eq!(s.companion().pending_pot, 0);
}

#[test]
fn retire_waits_while_a_period_is_open() {
    let mut s = Strat::new();
    let (a, _, _, p) = s.period_with_holders();
    s.plan(p).ok();
    let retire = |s: &mut Strat| {
        let ix = companion::retire_game(s.cranker.pubkey(), s.mint, HOOK);
        s.send(ix)
    };
    // A period paying: no retirement while it is open (and none due yet anyway).
    refused(&retire(&mut s), CompanionError::DrawPending);
    s.pay(p, &[a.pubkey()]).ok();
    // A payment moved `settled_at`: the 60 days count from it.
    let paid_at = s.w.env.now;
    s.w.env.warp(2 * DORMANT_SECS - 100);
    refused(&retire(&mut s), CompanionError::NotDue);
    s.w.env.warp(200);
    let tx = retire(&mut s);
    tx.ok();
    let ev: PotRetired = tx.event();
    assert_eq!(ev.idle_since, paid_at);
    assert_eq!(s.companion().pot_locked, 0);
}

// =============================================================================== limits

#[test]
fn a_full_payment_fits_a_packet_and_the_budgets() {
    let mut s = Strat::with(Some(STUDIO_KEY), |_, _| {}, true);
    let a = s.buyer(5 * SOL);
    let b = s.buyer(SOL);
    let c = s.buyer(2 * SOL);
    let d = s.buyer(3 * SOL);
    let p = s.start_round(&[&a, &b, &c, &d]);
    s.fund_pot();
    s.warp_into(p + 1, 10);
    // Each strategy call at its compute cap (the tester burns up to it, then answers).
    s.set_config(
        (st::MODE_BURN, u64::from(MAX_PLAN_CU) - 20_000),
        (st::MODE_BURN, u64::from(MAX_ENTITLE_CU) - 15_000),
    );
    let table = table_22(&s.w);
    let lut = s.w.env.put_lookup_table(Pubkey::new_unique(), &table);
    let cranker = s.cranker.insecure_clone();
    let plan = s.plan_ix(p);
    let tx = s.w.env.send_v0(
        &[compute_unit_limit(400_000), plan],
        &cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    println!(
        "plan_period at the cap, 2 extras: {} bytes, {} CU, height {}, heap {:?}",
        tx.size,
        tx.cu(),
        tx.max_height(),
        heap_peak(&tx)
    );
    assert!(tx.size <= PACKET);
    let owners = [a.pubkey(), b.pubkey(), c.pubkey(), d.pubkey()];
    let pay = s.pay_ix(p, &owners);
    let tx = s.w.env.send_v0(
        &[compute_unit_limit(1_400_000), pay],
        &cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    println!(
        "pay_strategy x4 at the cap, 2 extras: {} bytes, {} CU, height {}, trace {}, heap {:?}",
        tx.size,
        tx.cu(),
        tx.max_height(),
        tx.trace_len(),
        heap_peak(&tx)
    );
    assert!(tx.size <= PACKET, "pay_strategy is {} bytes", tx.size);
    assert_eq!(tx.event::<StrategyPaid>().payments.len(), 4);
    assert!(tx.max_height() <= 4);
    assert!(tx.trace_len() <= 64);
    assert!(tx.cu() < 1_400_000);
    if let Some(peak) = heap_peak(&tx) {
        assert!(peak < 28 * 1024, "pay_strategy's heap peaked at {peak} bytes");
    }
    // Four refused candidates (an answer over its bound each: an event each), the other worst case.
    let e = s.buyer(SOL);
    let f = s.buyer(SOL);
    let _ = (e, f);
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_MAX, 0));
    let refused_four = [b.pubkey(), c.pubkey(), d.pubkey(), a.pubkey()];
    // (They hold receipts now: fresh holders of the period are needed; reuse the paid ones, which
    // are refused before the call, and measure the over-bound path with new holders next period.)
    let pay = s.pay_ix(p, &refused_four);
    let tx = s.w.env.send_v0(
        &[compute_unit_limit(1_400_000), pay],
        &cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    println!("pay_strategy x4 all already paid: {} CU, heap {:?}", tx.cu(), heap_peak(&tx));
    s.enter(&[&a, &b, &c, &d]);
    s.warp_into(p + 2, 10);
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_PRO_RATA, 0));
    let plan = s.plan_ix(p + 1);
    s.w.env
        .send_v0(&[compute_unit_limit(400_000), plan], &cranker, &[], std::slice::from_ref(&lut))
        .ok();
    s.set_config((st::MODE_PRO_RATA, 0), (st::MODE_MAX, 0));
    let pay = s.pay_ix(p + 1, &owners);
    let tx = s.w.env.send_v0(
        &[compute_unit_limit(1_400_000), pay],
        &cranker,
        &[],
        std::slice::from_ref(&lut),
    );
    tx.ok();
    assert_eq!(tx.events::<CandidateRejected>().len(), 4);
    println!("pay_strategy x4 all over their bound: {} CU, heap {:?}", tx.cu(), heap_peak(&tx));
    if let Some(peak) = heap_peak(&tx) {
        assert!(peak < 28 * 1024, "pay_strategy's heap peaked at {peak} bytes");
    }
}

#[test]
fn a_strategy_pays_nobody_the_protocol_excludes_and_a_receipt_is_a_streaks() {
    // The receipt PDA is `["claimed", game, period, owner]`: one per game, so a strategy game's can
    // never be a streak's (another game account), and `close_receipt` works unchanged.
    let mint = Pubkey::new_unique();
    let game = companion::game_address(&mint);
    let other = companion::game_address(&Pubkey::new_unique());
    let owner = Pubkey::new_unique();
    assert_ne!(
        ShareReceipt::address(&game, 5, &owner).0,
        ShareReceipt::address(&other, 5, &owner).0
    );
    let _ = Companion::LEN;
}

// =============================================================================== Studio's starter

/// Studio's pro-rata starter (an Anchor program answering with `Result<PlanDecision>` and
/// `Result<Entitlement>`), immutable, from `prepare` to payments: what a Studio strategy is.
#[test]
fn studios_pro_rata_starter_runs_end_to_end() {
    use anchor_lang::{InstructionData, ToAccountMetas};
    const STARTER: Pubkey = strategy_pro_rata::ID;
    let mut w = World::new();
    w.env
        .svm
        .add_program(STARTER, &program_bytes("strategy_pro_rata"))
        .expect("load the starter");
    w.env.set_upgrade_authority(STARTER, None);
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let state = Pubkey::find_program_address(&[strategy_pro_rata::STATE_SEED, mint.as_ref()], &STARTER).0;
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
        ..strategy_args()
    };
    let ixs = vec![
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        lottery::prepare(launcher.pubkey(), mint, ROUND),
        prepare,
        companion::create_strategy_game(launcher.pubkey(), mint, game_args(), s, vec![], false, &[state]),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]);
    tx.ok();
    assert_eq!(tx.event::<StrategySet>().class, strategy_class::IMMUTABLE);
    let config = hook_config(&mut w, &launcher);
    let ix = launch_ix(&w, &launcher.pubkey(), &mint, &config);
    w.env.send_paid_by(&[ix], &launcher, &[&mint_kp]).ok();
    w.env.warp(31);
    let cranker = w.wallet_with_sol(5 * SOL);
    let mut st = Strat {
        w,
        mint,
        cranker,
        two: false,
    };
    let holders: Vec<Keypair> = (0..5).map(|_| st.buyer(SOL)).collect();
    let refs: Vec<&Keypair> = holders.iter().collect();
    let p = st.start_round(&refs);
    st.fund_pot();
    st.warp_into(p + 1, 10);
    let pot = st.companion().pending_pot;
    let plan = companion::plan_period(
        st.cranker.pubkey(),
        mint,
        HOOK,
        STARTER,
        st.w.launch(&mint).pool,
        &[state],
        p,
    );
    let tx = st.send(plan);
    tx.ok();
    let planned: PeriodPlanned = tx.event();
    assert_eq!(planned.budget, pot / 2);
    let total = st.header().total_of(p).unwrap();
    let owners: Vec<Pubkey> = holders.iter().map(|h| h.pubkey()).collect();
    let cranker = st.cranker.insecure_clone();
    let mut paid = 0u64;
    for chunk in owners.chunks(4) {
        let pay = companion::pay_strategy(cranker.pubkey(), mint, HOOK, STARTER, &[state], p, chunk);
        let tx = st
            .w
            .env
            .send_v0(&[compute_unit_limit(1_400_000), pay], &cranker, &[], &[]);
        tx.ok();
        println!("starter pay x{}: {} CU", chunk.len(), tx.cu());
        for pmt in tx.event::<StrategyPaid>().payments {
            paid += pmt.amount + pmt.bounty;
        }
    }
    // Five holders of about a fifth each: every one paid its share (none above a quarter).
    let expected: u64 = owners
        .iter()
        .map(|o| {
            let wt = st.weight(o, p);
            ((u128::from(planned.budget) * u128::from(wt) / u128::from(total)) as u64)
                .min(planned.budget / 4)
        })
        .sum();
    assert_eq!(paid, expected);
    assert!(paid <= planned.budget);
}
