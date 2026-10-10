//! Vectors for the SDK's mirror of phase 3a (`docs/phase3a.md` §13), as `game_vectors.rs` renders
//! `companion-games.json`: for fixed keys, every new instruction exactly as the Rust clients build
//! it, the new accounts as the programs serialize them, the constants, and the risk labels of
//! `bordrless_program_tests::risk` over a table of account states. When a file differs from what
//! the Rust reference computes, the test rewrites it and fails. The SDK keeps copies.
//!
//! - `vectors/timelock.json`: `hook_timelock`'s instructions, its `Timelock` and the loader's
//!   instructions around it;
//! - `vectors/strategy.json`: the companion's phase-3a instructions (strategies, attestations,
//!   audits tied to code, games taken with an attestation), `StrategyTerms`, `HookAttestation`,
//!   the strategy interface's encodings;
//! - `vectors/risk-labels.json`: label cases (accounts in, label out).

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AccountSerialize, AnchorSerialize};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::instructions::{
    AttestArgs, CreateGameArgs, GameKindArgs, HookStatusArgs, StrategyArgs,
};
use bordrless_companion::state::{GameKind, HookAttestation, HookStatus, Split, StrategyTerms};
use bordrless_hook::authority::{
    programdata_address, timelock_address, BPF_LOADER_2_ID, BPF_LOADER_UPGRADEABLE_ID,
    HOOK_TIMELOCK_ID, LOADER_V4_ID, MIN_ACCEPTED_DELAY, PROTOCOL_UPGRADE_AUTHORITY,
    STUDIO_UPGRADE_AUTHORITY,
};
use bordrless_program_tests::json::*;
use bordrless_program_tests::risk::{risk_label_of, RawAccount, RiskAccounts, RiskLabel};
use bordrless_strategy::{EntitleArgs, Entitlement, PlanArgs, PlanDecision};
use hook_timelock::{client as tl, loader, Timelock};

/// A fixed key: 32 bytes of `n`.
fn fixed(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

fn serialize<T: AccountSerialize>(account: &T, len: usize) -> Vec<u8> {
    let mut data = Vec::new();
    account.try_serialize(&mut data).expect("serialize");
    data.resize(len, 0);
    data
}

fn borsh<T: AnchorSerialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    v.serialize(&mut out).expect("borsh");
    out
}

// ------------------------------------------------------------------------------------- timelock

fn sample_timelock(program: Pubkey, author: Pubkey, pending: bool) -> Timelock {
    Timelock {
        version: hook_timelock::VERSION,
        bump: timelock_address(&program).1,
        program,
        programdata: programdata_address(&program),
        author,
        pending_author: Pubkey::default(),
        delay_secs: 5 * 86_400,
        finalized: false,
        pending_buffer: if pending { fixed(40) } else { Pubkey::default() },
        pending_hash: if pending { [0xab; 32] } else { [0; 32] },
        pending_len: if pending { 123_456 } else { 0 },
        proposed_at: if pending { 1_800_000_000 } else { 0 },
        eta: if pending { 1_800_432_000 } else { 0 },
        upgrades: 2,
        created_at: 1_799_000_000,
        last_upgraded_at: 1_799_500_000,
        reserved: [0; 32],
    }
}

fn timelock_vectors() -> Vec<(&'static str, J)> {
    let (payer, authority, program, author, buffer, sender, new_author) = (
        fixed(1),
        fixed(2),
        fixed(3),
        fixed(4),
        fixed(5),
        fixed(6),
        fixed(7),
    );
    let instructions = J::Arr(vec![
        ix(
            "register",
            &tl::register(payer, authority, program, MIN_ACCEPTED_DELAY, author),
        ),
        ix("propose", &tl::propose(author, program, buffer, 207_681)),
        ix("cancel", &tl::cancel(author, program, buffer)),
        ix("execute", &tl::execute(sender, program, buffer, author)),
        ix("expire", &tl::expire(sender, program, buffer, author)),
        ix("reclaimBuffer", &tl::reclaim_buffer(author, program, buffer)),
        ix("lengthen", &tl::lengthen(author, program, 30 * 86_400)),
        ix("proposeAuthor", &tl::propose_author(author, program, new_author)),
        ix("acceptAuthor", &tl::accept_author(new_author, program)),
        ix("finalize", &tl::finalize(author, program)),
        ix(
            "loaderSetAuthority",
            &loader::set_authority(buffer, author, Some(tl::timelock_address(&program))),
        ),
        ix(
            "loaderExtendProgram",
            &loader::extend_program(programdata_address(&program), program, payer, 10_240),
        ),
    ]);
    let accounts = J::Arr(
        [false, true]
            .into_iter()
            .map(|pending| {
                let t = sample_timelock(program, author, pending);
                J::Obj(vec![
                    ("pending", J::Bool(pending)),
                    ("address", key(&tl::timelock_address(&program))),
                    ("data", hex(&serialize(&t, Timelock::LEN))),
                ])
            })
            .collect(),
    );
    vec![
        (
            "keys",
            J::Obj(vec![
                ("payer", key(&payer)),
                ("authority", key(&authority)),
                ("program", key(&program)),
                ("author", key(&author)),
                ("buffer", key(&buffer)),
                ("sender", key(&sender)),
                ("newAuthor", key(&new_author)),
            ]),
        ),
        (
            "constants",
            J::Obj(vec![
                ("programId", key(&hook_timelock::ID)),
                ("eventAuthority", key(&tl::event_authority())),
                ("minDelaySecs", num(hook_timelock::MIN_DELAY)),
                ("maxDelaySecs", num(bordrless_hook::authority::MAX_DELAY)),
                ("executeWindowSecs", num(hook_timelock::EXECUTE_WINDOW)),
                ("maxCodeLen", num(hook_timelock::MAX_CODE_LEN)),
                ("timelockLen", num(Timelock::LEN as i64)),
                ("timelockAddress", key(&tl::timelock_address(&program))),
                ("programdataAddress", key(&programdata_address(&program))),
            ]),
        ),
        ("instructions", instructions),
        ("accounts", accounts),
    ]
}

#[test]
fn the_timelock_vectors_are_current() {
    check_vectors("timelock.json", &timelock_vectors());
}

// ------------------------------------------------------------------------------------- strategy

fn game_args(hook: Pubkey) -> CreateGameArgs {
    CreateGameArgs {
        kind: GameKind::Strategy,
        hook,
        split: Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        pot_bps: 7_000,
        round_secs: 3_600,
        min_pot: 100_000_000,
        prize_bps: 0,
        claim_window_secs: 600,
        max_attempts: 0,
    }
}

fn strategy_args(strategy: Pubkey) -> StrategyArgs {
    StrategyArgs {
        strategy,
        budget_bps: 5_000,
        max_share_bps: 2_500,
        max_per_tx: 4,
        plan_cu_max: 150_000,
        entitle_cu_max: 60_000,
        min_weight: 1_000,
    }
}

fn attest_args() -> AttestArgs {
    AttestArgs {
        build_hash: [0x11; 32],
        source_hash: [0x22; 32],
        template_commit: [0x33; 20],
        sim_version: 3,
        sim_pass: true,
        cut_max_bps: 250,
        cap_bps: 500,
        review: REVIEW_WARN,
        kind: 2,
    }
}

fn sample_terms(mint: Pubkey, strategy: Pubkey) -> StrategyTerms {
    StrategyTerms {
        version: STRATEGY_VERSION,
        bump: StrategyTerms::address(&mint).1,
        game: companion::game_address(&mint),
        mint,
        strategy,
        status_bump: HookStatus::address(&strategy).1,
        extras: [fixed(30), fixed(31)],
        n_extras: 2,
        budget_bps: 5_000,
        max_share_bps: 2_500,
        max_per_tx: 4,
        plan_cu_max: 150_000,
        entitle_cu_max: 60_000,
        periods_planned: 7,
        paid_total: 12_345_678_901,
        last_plan_at: 1_800_003_600,
        paid_at_active: 2_000_000,
        audit_ok: true,
        audit_slot: 4_242,
        reserved: [0; 15],
    }
}

fn sample_attestation(program: Pubkey) -> HookAttestation {
    HookAttestation {
        version: ATTESTATION_VERSION,
        bump: HookAttestation::address(&program).1,
        program,
        build_hash: [0x11; 32],
        source_hash: [0x22; 32],
        template_commit: [0x33; 20],
        sim_version: 3,
        sim_pass: true,
        cut_max_bps: 250,
        cap_bps: 500,
        review: REVIEW_WARN,
        kind: 2,
        programdata_slot: 400_000_123,
        attested_at: 1_800_000_000,
        attester: STUDIO_ATTESTER,
        revoked: false,
        revoked_at: 0,
        reserved: [0; 32],
    }
}

fn sample_status(hook: Pubkey, audited: bool, hash: [u8; 32]) -> HookStatus {
    HookStatus {
        version: HOOK_STATUS_VERSION,
        bump: HookStatus::address(&hook).1,
        hook,
        audited,
        pot_cap: if audited { 0 } else { 2_000_000_000 },
        blocked: false,
        updated_at: 1_800_000_000,
        updated_by: fixed(9),
        reserved: hash,
    }
}

fn strategy_vectors() -> Vec<(&'static str, J)> {
    let (payer, mint, hook, strategy, cranker, pool, owner_a, owner_b, authority) = (
        fixed(11),
        fixed(12),
        lottery_hook::ID,
        fixed(14),
        fixed(15),
        fixed(16),
        fixed(17),
        fixed(18),
        fixed(19),
    );
    let extras = [fixed(30), fixed(31)];
    let studio_hook = fixed(20);
    let instructions = J::Arr(vec![
        ix(
            "createStrategyGame",
            &companion::create_strategy_game(
                payer,
                mint,
                game_args(hook),
                strategy_args(strategy),
                vec![],
                false,
                &extras,
            ),
        ),
        ix(
            "createStrategyGameTimelocked",
            &companion::create_strategy_game(
                payer,
                mint,
                game_args(studio_hook),
                strategy_args(strategy),
                companion::vetting_accounts(&studio_hook, false),
                true,
                &extras[..1],
            ),
        ),
        ix(
            "planPeriod",
            &companion::plan_period(cranker, mint, hook, strategy, pool, &extras, 500_123),
        ),
        ix(
            "payStrategy",
            &companion::pay_strategy(
                cranker,
                mint,
                hook,
                strategy,
                &extras,
                500_123,
                &[owner_a, owner_b],
            ),
        ),
        ix("attest", &companion::attest(STUDIO_ATTESTER, studio_hook, attest_args())),
        ix("revoke", &companion::revoke(authority, studio_hook)),
        ix(
            "setHookStatusV2",
            &companion::set_hook_status_v2(
                authority,
                studio_hook,
                HookStatusArgs {
                    audited: true,
                    pot_cap: 0,
                    blocked: false,
                },
                [0x44; 32],
            ),
        ),
        ix(
            "setHookStatusChecked",
            &companion::set_hook_status_checked(
                authority,
                studio_hook,
                HookStatusArgs {
                    audited: false,
                    pot_cap: 1_000_000_000,
                    blocked: true,
                },
            ),
        ),
        ix(
            "createGameV2Attested",
            &companion::create_game_v2_attested(
                payer,
                mint,
                CreateGameArgs {
                    kind: GameKind::Streak,
                    hook: studio_hook,
                    split: Split {
                        buyback_bps: 3_000,
                        holders_bps: 0,
                        beneficiary_bps: 0,
                    },
                    pot_bps: 7_000,
                    round_secs: 86_400,
                    min_pot: 100_000_000,
                    prize_bps: 10_000,
                    claim_window_secs: 3_600,
                    max_attempts: 0,
                },
                GameKindArgs {
                    min_streak_secs: 3_600,
                    min_weight: 1,
                    ..GameKindArgs::default()
                },
                true,
            ),
        ),
        ix(
            "createGameAttested",
            &companion::create_game_attested(
                payer,
                mint,
                CreateGameArgs {
                    kind: GameKind::Lottery,
                    hook: studio_hook,
                    split: Split {
                        buyback_bps: 3_000,
                        holders_bps: 0,
                        beneficiary_bps: 0,
                    },
                    pot_bps: 7_000,
                    round_secs: 3_600,
                    min_pot: 100_000_000,
                    prize_bps: 10_000,
                    claim_window_secs: 300,
                    max_attempts: 6,
                },
                false,
            ),
        ),
    ]);
    let plan = PlanArgs {
        version: bordrless_strategy::ARGS_VERSION,
        mint,
        period: 500_123,
        period_start: 1_800_442_800,
        period_end: 1_800_446_400,
        total: 9_876_543_210,
        pot: 4_000_000_000,
        budget_max: 2_000_000_000,
        periods_planned: 3,
        paid_total: 5_555_555,
        now: 1_800_446_410,
    };
    let entitle = EntitleArgs {
        version: bordrless_strategy::ARGS_VERSION,
        mint,
        period: 500_123,
        owner: owner_a,
        balance: 1_000_000_000,
        weight: 900_000_000,
        since: 1_800_000_000,
        total: 9_876_543_210,
        budget: 2_000_000_000,
        paid: 100,
        max_amount: 500_000_000,
        now: 1_800_446_420,
    };
    let interface = J::Obj(vec![
        ("plan", hex(&bordrless_strategy::PLAN)),
        ("entitle", hex(&bordrless_strategy::ENTITLE)),
        ("planArgs", hex(&borsh(&plan))),
        ("entitleArgs", hex(&borsh(&entitle))),
        ("planDecision", hex(&borsh(&PlanDecision { budget: 1_234_567 }))),
        ("entitlement", hex(&borsh(&Entitlement { amount: 7_654_321 }))),
        (
            "registryAddress",
            key(&bordrless_strategy::registry_address(&strategy, &mint).0),
        ),
        ("registrySeed", s(String::from_utf8_lossy(bordrless_strategy::REGISTRY_SEED))),
        ("maxExtras", num(bordrless_strategy::MAX_EXTRAS as i64)),
    ]);
    let accounts = J::Obj(vec![
        (
            "strategyTerms",
            J::Obj(vec![
                ("address", key(&companion::strategy_terms_address(&mint))),
                (
                    "data",
                    hex(&serialize(&sample_terms(mint, strategy), StrategyTerms::LEN)),
                ),
            ]),
        ),
        (
            "attestation",
            J::Obj(vec![
                ("address", key(&companion::attestation_address(&studio_hook))),
                (
                    "data",
                    hex(&serialize(&sample_attestation(studio_hook), HookAttestation::LEN)),
                ),
            ]),
        ),
        (
            "hookStatusWithHash",
            J::Obj(vec![
                ("address", key(&companion::hook_status_address(&studio_hook))),
                (
                    "data",
                    hex(&serialize(
                        &sample_status(studio_hook, true, [0x55; 32]),
                        HookStatus::LEN,
                    )),
                ),
            ]),
        ),
    ]);
    let constants = J::Obj(vec![
        ("studioAttester", key(&STUDIO_ATTESTER)),
        ("hookTimelockId", key(&HOOK_TIMELOCK_ID)),
        ("strategySeed", s("strategy")),
        ("attestSeed", s("attest")),
        ("maxBudgetBps", num(MAX_STRATEGY_BUDGET_BPS)),
        ("maxShareBps", num(MAX_STRATEGY_SHARE_BPS)),
        ("maxPerTx", num(MAX_STRATEGY_PER_TX)),
        ("maxPlanCu", num(MAX_PLAN_CU)),
        ("maxEntitleCu", num(MAX_ENTITLE_CU)),
        ("strategyTermsLen", num(StrategyTerms::LEN as i64)),
        ("attestationLen", num(HookAttestation::LEN as i64)),
        ("gameKindStrategy", num(GameKind::Strategy as u8)),
    ]);
    vec![
        (
            "keys",
            J::Obj(vec![
                ("payer", key(&payer)),
                ("mint", key(&mint)),
                ("hook", key(&hook)),
                ("strategy", key(&strategy)),
                ("cranker", key(&cranker)),
                ("pool", key(&pool)),
                ("ownerA", key(&owner_a)),
                ("ownerB", key(&owner_b)),
                ("authority", key(&authority)),
                ("studioHook", key(&studio_hook)),
                ("extras", J::Arr(extras.iter().map(key).collect())),
            ]),
        ),
        ("constants", constants),
        ("interface", interface),
        ("accounts", accounts),
        ("instructions", instructions),
    ]
}

#[test]
fn the_strategy_vectors_are_current() {
    check_vectors("strategy.json", &strategy_vectors());
}

// ------------------------------------------------------------------------------------- risk labels

fn programdata_raw(authority: Option<Pubkey>, slot: u64, code: &[u8]) -> RawAccount {
    let mut data = vec![0u8; 45];
    data[..4].copy_from_slice(&3u32.to_le_bytes());
    data[4..12].copy_from_slice(&slot.to_le_bytes());
    if let Some(a) = authority {
        data[12] = 1;
        data[13..45].copy_from_slice(a.as_ref());
    }
    data.extend_from_slice(code);
    data.extend([0u8; 16]);
    RawAccount {
        owner: BPF_LOADER_UPGRADEABLE_ID,
        data,
        executable: false,
    }
}

fn program_raw(program: &Pubkey) -> RawAccount {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(programdata_address(program).as_ref());
    RawAccount {
        owner: BPF_LOADER_UPGRADEABLE_ID,
        data,
        executable: true,
    }
}

fn timelock_raw(program: &Pubkey, author: Pubkey, pending: bool) -> RawAccount {
    RawAccount {
        owner: HOOK_TIMELOCK_ID,
        data: serialize(&sample_timelock(*program, author, pending), Timelock::LEN),
        executable: false,
    }
}

fn status_raw(hook: &Pubkey, audited: bool, hash: [u8; 32], blocked: bool, cap: u64) -> RawAccount {
    let mut s = sample_status(*hook, audited, hash);
    s.blocked = blocked;
    if !audited {
        s.pot_cap = cap;
    }
    RawAccount {
        owner: bordrless_companion::ID,
        data: serialize(&s, HookStatus::LEN),
        executable: false,
    }
}

fn attestation_raw(program: &Pubkey, build: [u8; 32], slot: u64, revoked: bool) -> RawAccount {
    let mut a = sample_attestation(*program);
    a.build_hash = build;
    a.programdata_slot = slot;
    a.revoked = revoked;
    RawAccount {
        owner: bordrless_companion::ID,
        data: serialize(&a, HookAttestation::LEN),
        executable: false,
    }
}

fn raw_json(r: &Option<RawAccount>) -> J {
    match r {
        None => J::Null,
        Some(a) => J::Obj(vec![
            ("owner", key(&a.owner)),
            ("executable", J::Bool(a.executable)),
            ("data", hex(&a.data)),
        ]),
    }
}

fn label_json(l: &RiskLabel) -> J {
    J::Obj(vec![
        ("class", s(l.class)),
        ("delaySecs", opt(l.delay_secs.map(num))),
        ("author", opt(l.author.as_ref().map(key))),
        (
            "pending",
            opt(l.pending.as_ref().map(|(h, eta, b)| {
                J::Obj(vec![("hash", s(h.clone())), ("eta", num(*eta)), ("buffer", key(b))])
            })),
        ),
        (
            "timelockProgramUpgradeable",
            opt(l.timelock_program_upgradeable.map(J::Bool)),
        ),
        ("audited", s(l.audited)),
        ("blocked", J::Bool(l.blocked)),
        ("potCap", opt(l.pot_cap.map(big))),
        ("provenance", s(l.provenance)),
        (
            "studio",
            opt(l.studio.as_ref().map(|(h, pass, cut, current)| {
                J::Obj(vec![
                    ("buildHash", s(h.clone())),
                    ("simPass", J::Bool(*pass)),
                    ("cutMaxBps", num(*cut)),
                    ("current", J::Bool(*current)),
                ])
            })),
        ),
        ("severity", s(l.severity)),
        ("words", s(l.words.clone())),
    ])
}

fn risk_vectors() -> Vec<(&'static str, J)> {
    let program = fixed(50);
    let code = vec![0x7fu8, 0x45, 0x4c, 0x46, 1, 2, 3, 4, 5];
    let code_hash = bordrless_program_tests::timelock::executable_hash(&code);
    let other_hash = [0x99u8; 32];
    let author = fixed(51);
    let tl_key = timelock_address(&program).0;
    let base = |authority: Option<Pubkey>| RiskAccounts {
        program_id: program,
        program: Some(program_raw(&program)),
        programdata: Some(programdata_raw(authority, 100, &code)),
        timelock: None,
        timelock_programdata: Some(programdata_raw(Some(fixed(60)), 5, &[1])),
        status: None,
        attestation: None,
    };
    let now = 1_800_100_000i64;
    let mut cases: Vec<(&'static str, RiskAccounts, Option<[u8; 32]>)> = vec![];
    cases.push(("missing", RiskAccounts { program: None, ..base(None) }, None));
    cases.push(("immutable unchecked", base(None), None));
    cases.push((
        "immutable attested (same slot)",
        RiskAccounts {
            attestation: Some(attestation_raw(&program, code_hash, 100, false)),
            ..base(None)
        },
        None,
    ));
    cases.push((
        "managed by Studio, attested, slot moved, hash matches",
        RiskAccounts {
            attestation: Some(attestation_raw(&program, code_hash, 90, false)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        Some(code_hash),
    ));
    cases.push((
        "managed by Studio, attestation stale",
        RiskAccounts {
            attestation: Some(attestation_raw(&program, other_hash, 90, false)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        Some(code_hash),
    ));
    cases.push((
        "managed by Studio, attestation revoked",
        RiskAccounts {
            attestation: Some(attestation_raw(&program, code_hash, 100, true)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        None,
    ));
    cases.push((
        "donated to Studio's key, no attestation",
        base(Some(STUDIO_UPGRADE_AUTHORITY)),
        None,
    ));
    cases.push((
        "the protocol's key",
        base(Some(PROTOCOL_UPGRADE_AUTHORITY)),
        None,
    ));
    cases.push(("author-upgradeable", base(Some(author)), None));
    cases.push((
        "timelocked, the timelock still Bordrless-upgradeable",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, false)),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "timelocked, the timelock finalized",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, false)),
            timelock_programdata: Some(programdata_raw(None, 5, &[1])),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "timelocked with a proposal pending",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, true)),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "timelocked with a proposal past its eta",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, true)),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "timelocked, its timelock missing",
        base(Some(tl_key)),
        None,
    ));
    cases.push((
        "handed to its timelock address, lamports there but no Timelock",
        RiskAccounts {
            timelock: Some(RawAccount {
                owner: Pubkey::default(),
                data: vec![],
                executable: false,
            }),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "closed (its ProgramData gone)",
        RiskAccounts {
            programdata: None,
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        None,
    ));
    cases.push((
        "timelocked, attested, other code proposed",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, true)),
            attestation: Some(attestation_raw(&program, code_hash, 100, false)),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "timelocked, attested",
        RiskAccounts {
            timelock: Some(timelock_raw(&program, author, false)),
            attestation: Some(attestation_raw(&program, code_hash, 100, false)),
            ..base(Some(tl_key))
        },
        None,
    ));
    cases.push((
        "audited, hash current",
        RiskAccounts {
            status: Some(status_raw(&program, true, code_hash, false, 0)),
            ..base(None)
        },
        Some(code_hash),
    ));
    cases.push((
        "audited, then handed to a timelock (hash still the code's)",
        RiskAccounts {
            status: Some(status_raw(&program, true, code_hash, false, 0)),
            timelock: Some(timelock_raw(&program, author, false)),
            ..base(Some(tl_key))
        },
        Some(code_hash),
    ));
    cases.push((
        "audited by v1 (no hash)",
        RiskAccounts {
            status: Some(status_raw(&program, true, [0; 32], false, 0)),
            ..base(Some(PROTOCOL_UPGRADE_AUTHORITY))
        },
        Some(code_hash),
    ));
    cases.push((
        "audited, the code changed since",
        RiskAccounts {
            status: Some(status_raw(&program, true, other_hash, false, 0)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        Some(code_hash),
    ));
    cases.push((
        "capped at 2 SOL",
        RiskAccounts {
            status: Some(status_raw(&program, false, [0; 32], false, 2_000_000_000)),
            attestation: Some(attestation_raw(&program, code_hash, 100, false)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        None,
    ));
    cases.push((
        "blocked",
        RiskAccounts {
            status: Some(status_raw(&program, false, [0; 32], true, DEFAULT_POT_CAP)),
            ..base(Some(STUDIO_UPGRADE_AUTHORITY))
        },
        None,
    ));
    cases.push((
        "loader 2",
        RiskAccounts {
            program: Some(RawAccount {
                owner: BPF_LOADER_2_ID,
                data: code.clone(),
                executable: true,
            }),
            programdata: None,
            ..base(None)
        },
        None,
    ));
    cases.push((
        "loader v4, not finalized",
        RiskAccounts {
            program: Some(RawAccount {
                owner: LOADER_V4_ID,
                data: {
                    let mut d = vec![0u8; 48];
                    d[8..40].copy_from_slice(author.as_ref());
                    d
                },
                executable: true,
            }),
            programdata: None,
            ..base(None)
        },
        None,
    ));
    // The pending case past its eta reads at a later clock.
    let rows: Vec<J> = cases
        .into_iter()
        .map(|(name, a, hash)| {
            let at = if name.contains("past its eta") {
                1_800_432_001
            } else {
                now
            };
            let label = risk_label_of(&a, at, hash);
            J::Obj(vec![
                ("name", s(name)),
                ("now", num(at)),
                ("executableHash", opt(hash.map(|h| hex(&h)))),
                (
                    "accounts",
                    J::Obj(vec![
                        ("programId", key(&a.program_id)),
                        ("program", raw_json(&a.program)),
                        ("programdata", raw_json(&a.programdata)),
                        ("timelock", raw_json(&a.timelock)),
                        ("timelockProgramdata", raw_json(&a.timelock_programdata)),
                        ("status", raw_json(&a.status)),
                        ("attestation", raw_json(&a.attestation)),
                    ]),
                ),
                ("label", label_json(&label)),
            ])
        })
        .collect();
    vec![
        (
            "constants",
            J::Obj(vec![
                ("defaultPotCap", big(DEFAULT_POT_CAP)),
                (
                    "bordrlessHooks",
                    J::Arr(
                        bordrless_program_tests::risk::BORDRLESS_HOOKS
                            .iter()
                            .map(key)
                            .collect(),
                    ),
                ),
                ("hookTimelockId", key(&HOOK_TIMELOCK_ID)),
                ("companionId", key(&bordrless_companion::ID)),
            ]),
        ),
        ("cases", J::Arr(rows)),
    ]
}

#[test]
fn the_risk_label_vectors_are_current() {
    check_vectors("risk-labels.json", &risk_vectors());
}

#[test]
fn the_labels_say_what_the_spec_says() {
    let rows = risk_vectors();
    let J::Arr(cases) = &rows[1].1 else {
        panic!("cases")
    };
    assert!(cases.len() >= 20);
    let words = |a: &RiskAccounts, h| risk_label_of(a, 1_800_100_000, h).words;
    let program = fixed(50);
    let a = RiskAccounts {
        program_id: program,
        program: None,
        programdata: None,
        timelock: None,
        timelock_programdata: None,
        status: None,
        attestation: None,
    };
    assert_eq!(
        words(&a, None),
        "This hook's program is gone: every transfer fails."
    );
    assert_eq!(bordrless_program_tests::risk::iso_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(
        bordrless_program_tests::risk::iso_utc(1_800_432_000),
        "2027-01-20T08:00:00Z"
    );
    assert_eq!(bordrless_program_tests::risk::delay_words(86_400), "1 day");
    assert_eq!(bordrless_program_tests::risk::delay_words(3 * 86_400), "3 days");
    assert_eq!(bordrless_program_tests::risk::delay_words(7_200), "2 hours");
}
