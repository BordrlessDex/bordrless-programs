//! Independent audit of phase 3a, custody and authority, round 1 (log:
//! `bordrless-games-work/log-3a-audit-custody-r1.md`).
//!
//! Each finding has a PoC named `f<N>_...`; every `c_...` test is a control: an attack that was
//! tried and is refused (the "checked, fine" list of the log).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::instructions::{
    AttestArgs, CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs,
};
use bordrless_companion::state::{GameKind, HookAttestation, HookStatus, Split};
use bordrless_hook::authority::{
    programdata_address, trimmed_len, BPF_LOADER_UPGRADEABLE_ID, MIN_ACCEPTED_DELAY,
};
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::attest::put_attestation_account;
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::SOL;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::*;
use hook_timelock::client as tl;
use hook_timelock::loader;
use hook_timelock::{Timelock, TimelockError, MIN_DELAY};
use solana_keypair::Keypair;
use solana_signer::Signer;

// =============================================================================== shared: companion A

const JACKPOT: Pubkey = studio_jackpot::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const ATTESTER: Pubkey = STUDIO_ATTESTER;

fn ccode(e: CompanionError) -> u32 {
    u32::from(e)
}

fn tcode(e: TimelockError) -> u32 {
    u32::from(e)
}

/// The studio jackpot starter loaded and upgradeable by Studio's key, sigverify off (the attester's
/// key is a constant the suite does not hold).
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

fn att_args(env: &Env, program: &Pubkey) -> AttestArgs {
    AttestArgs {
        build_hash: programdata_hash(env, program),
        source_hash: [5; 32],
        template_commit: [6; 20],
        sim_version: 3,
        sim_pass: true,
        cut_max_bps: 0,
        cap_bps: 0,
        review: REVIEW_PASS,
        kind: 1,
    }
}

fn attest(env: &mut Env, program: Pubkey, a: AttestArgs) -> Tx {
    send_as(env, &[companion::attest(ATTESTER, program, a)], &[], &[ATTESTER])
}

/// The attester attests `program`'s code as it is now.
fn attest_now(env: &mut Env, program: Pubkey) -> Tx {
    let a = att_args(env, &program);
    attest(env, program, a)
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn jackpot_args() -> (CreateGameArgs, GameKindArgs) {
    (
        CreateGameArgs {
            kind: GameKind::Jackpot,
            hook: JACKPOT,
            split: Split {
                buyback_bps: 3_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            },
            pot_bps: 7_000,
            round_secs: 0,
            min_pot: 100_000_000,
            prize_bps: 5_000,
            claim_window_secs: 0,
            max_attempts: 0,
        },
        GameKindArgs {
            timer_secs: studio_jackpot::TIMER_SECS,
            min_tokens: studio_jackpot::MIN_TOKENS,
            ..GameKindArgs::default()
        },
    )
}

fn prepare_ix(payer: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: JACKPOT,
        accounts: studio_jackpot::accounts::Prepare {
            payer,
            mint,
            state: bordrless_game::state_address(&JACKPOT, &mint).0,
            registry: bordrless_hook::hook_accounts_address(&JACKPOT, &mint).0,
            system_program: bordrless_program_tests::SYSTEM_PROGRAM_ID,
        }
        .to_account_metas(None),
        data: studio_jackpot::instruction::Prepare {}.data(),
    }
}

/// `create_game_v2` of a jackpot on the starter (with its vetting accounts), the game ix rewritten
/// by `tweak` first; a 1.4M compute budget in front (`send_paid_by`).
fn make_game_with(w: &mut World, timelocked: bool, tweak: impl FnOnce(&mut Instruction)) -> Tx {
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (a, k) = jackpot_args();
    let mut game = companion::create_game_v2_attested(launcher.pubkey(), mint, a, k, timelocked);
    tweak(&mut game);
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(launcher.pubkey(), mint),
        game,
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
}

fn make_game(w: &mut World, timelocked: bool) -> Tx {
    make_game_with(w, timelocked, |_| {})
}

/// Anyone grows `program`'s ProgramData by `bytes` (the loader's permissionless `ExtendProgram`),
/// then the clock moves a slot.
fn extend(env: &mut Env, program: Pubkey, bytes: u32) -> Tx {
    let griefer = env.funded(200 * SOL);
    let tx = env.send(
        &[loader::extend_program(
            programdata_address(&program),
            program,
            griefer.pubkey(),
            bytes,
        )],
        &[&griefer],
    );
    env.warp(1);
    tx
}

fn exceeded_compute(tx: &Tx) -> bool {
    tx.logs()
        .iter()
        .any(|l| l.contains("exceeded CUs meter") || l.contains("exceeded maximum compute"))
        || format!("{:?}", tx.err()).contains("ComputationalBudgetExceeded")
}

fn status_args(audited: bool) -> HookStatusArgs {
    HookStatusArgs {
        audited,
        pot_cap: DEFAULT_POT_CAP,
        blocked: false,
    }
}

// =============================================================================== F1

/// F1: anyone can grow an attested hook's ProgramData (permissionless `ExtendProgram`), which moves
/// its slot off the attestation's, so every reader recomputes the executable hash on chain. That
/// recomputation scans the trailing zeros 8 bytes at a time (`trimmed_len`): enough zeros and the
/// hash no longer fits a transaction. From then on no game can be made with the hook without a
/// status (`attestation_current`), Studio can't re-attest it (`attest` hashes too) and
/// `set_hook_status_v2` can't audit it. Only a `set_hook_status` (v1) status gets it back.
#[test]
fn f1_extend_griefing_makes_an_attested_hook_unusable_and_unattestable() {
    let mut w = att_world();
    attest_now(&mut w.env, JACKPOT).ok();
    let base = make_game(&mut w, false);
    base.ok();
    let code_len = w.env.account(&programdata_address(&JACKPOT)).unwrap().data.len() - 45;
    // A small extension: the slot moves, the hash is recomputed (the doc's "readers fall back to
    // the hash"): still current, at a price.
    extend(&mut w.env, JACKPOT, 10_240).ok();
    let small = make_game(&mut w, false);
    small.ok();
    extend(&mut w.env, JACKPOT, 102_400).ok();
    let tenth_mib = make_game(&mut w, false);
    tenth_mib.ok();
    let per_kib = (tenth_mib.cu() as f64 - small.cu() as f64) / 90.0;
    println!(
        "create_game_v2 (attested, {code_len} B of code): fast path {} CU; after +10 KiB {} CU; \
         after +110 KiB {} CU: ~{per_kib:.0} CU per KiB of zeros scanned",
        base.cu(),
        small.cu(),
        tenth_mib.cu()
    );
    // The griefer adds 1 MiB in one transaction (rent locked in the victim's ProgramData).
    let big: u32 = 1_024 * 1_024;
    let rent = w.env.rent(big as usize);
    extend(&mut w.env, JACKPOT, big).ok();
    println!(
        "griefing extension of {big} B costs {:.2} SOL of rent",
        rent as f64 / 1e9
    );
    // Fixed (round 1): the zero tail is scanned in chunks by `sol_memcmp`, so the hash still fits,
    // up to the 10 MiB a ProgramData can grow to.
    let one_mib = make_game(&mut w, false);
    one_mib.ok();
    let len = w.env.account(&programdata_address(&JACKPOT)).unwrap().data.len();
    let to_max = (10 * 1_024 * 1_024 - len) as u32;
    extend(&mut w.env, JACKPOT, to_max).ok();
    assert_eq!(
        w.env.account(&programdata_address(&JACKPOT)).unwrap().data.len(),
        10 * 1_024 * 1_024
    );
    let ten_mib = make_game(&mut w, false);
    ten_mib.ok();
    println!(
        "fixed: create_game_v2 after +1 MiB {} CU, at the 10 MiB maximum {} CU",
        one_mib.cu(),
        ten_mib.cu()
    );
    assert!(ten_mib.cu() < 400_000);
    // Studio re-attests it and the protocol audits it with v2: both hash the ProgramData.
    let tx = attest_now(&mut w.env, JACKPOT);
    tx.ok();
    println!("fixed: attest at 10 MiB {} CU", tx.cu());
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &JACKPOT);
    let tx = w.env.send(
        &[companion::set_hook_status_v2(
            deployer.pubkey(),
            JACKPOT,
            status_args(true),
            hash,
        )],
        &[&deployer],
    );
    tx.ok();
    println!("fixed: set_hook_status_v2 at 10 MiB {} CU", tx.cu());
    let _ = exceeded_compute;
}

// =============================================================================== F2

/// F2: a revocation by the companion's upgrade authority is undone by the attester: `attest`
/// (`init_if_needed`) rewrites `revoked = false`. If the hot attester key is what the protocol is
/// revoking against (owner decision 8), the revocation does not hold.
#[test]
fn f2_a_protocol_revocation_is_undone_by_a_re_attestation() {
    let mut w = att_world();
    let a = att_args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a.clone()).ok();
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send(&[companion::revoke(deployer.pubkey(), JACKPOT)], &[&deployer])
        .ok();
    make_game(&mut w, false).expect_code(ccode(CompanionError::HookNotAttested));
    // Fixed (round 1): the attester (or whoever holds its key) can't write the same build again.
    w.env.warp(1);
    attest(&mut w.env, JACKPOT, a)
        .expect_code(ccode(CompanionError::RevokedByProtocol));
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert!(at.revoked);
    make_game(&mut w, false).expect_code(ccode(CompanionError::HookNotAttested));
    // A revocation by the attester itself is the attester's to lift.
    let mut w = att_world();
    let a = att_args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a.clone()).ok();
    send_as(&mut w.env, &[companion::revoke(ATTESTER, JACKPOT)], &[], &[ATTESTER]).ok();
    w.env.warp(1);
    attest(&mut w.env, JACKPOT, a).ok();
    make_game(&mut w, false).ok();
}

// =============================================================================== F3

/// F3: `set_hook_status` (v1) still audits a program anyone can upgrade, when its ProgramData is
/// simply not passed (owner decision 5 says such a program is refused an audit). An audit is final
/// and lifts the 10 SOL cap and the block switch for good.
#[test]
fn f3_v1_audits_an_author_upgradeable_hook_when_its_programdata_is_left_out() {
    let mut w = att_world();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &JACKPOT);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                JACKPOT,
                status_args(true),
                hash,
            )],
            &[&deployer],
        )
        .expect_code(ccode(CompanionError::AuditNeedsFixedCode));
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                JACKPOT,
                status_args(true),
            )],
            &[&deployer],
        )
        .ok();
    let s: HookStatus = w.env.read(&companion::hook_status_address(&JACKPOT));
    assert!(s.audited && s.audited_hash() == [0; 32]);
    // The author's hook now enters a game uncapped (and can never be blocked: an audit is final).
    let tx = make_game(&mut w, false);
    tx.ok();
    let set: bordrless_companion::events::GameKindSet = tx.event();
    assert!(set.audited);
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                JACKPOT,
                HookStatusArgs {
                    audited: false,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked: true,
                },
            )],
            &[&deployer],
        )
        .expect_code(ccode(CompanionError::BadHookStatus));
}

// =============================================================================== F4

/// F4: an attestation's `kind` is not read: a token-hook (0) or strategy (2) attestation vets a
/// game hook, though Studio's game-hook checks (the simulator's "doesn't refuse the companion's
/// buyback") may never have run on it.
#[test]
fn f4_an_attestation_of_another_kind_vets_a_game_hook() {
    // Fixed (round 1): only a game-hook attestation (kind 1) vets a game hook.
    for kind in [0u8, 2] {
        let mut w = att_world();
        let mut a = att_args(&w.env, &JACKPOT);
        a.kind = kind;
        attest(&mut w.env, JACKPOT, a).ok();
        make_game(&mut w, false).expect_code(ccode(CompanionError::HookNotAttested));
    }
}

// =============================================================================== F5

/// F5: a timelocked game hook is vetted on its current code only: a game is made while a proposal
/// of other (unattested) code is pending and already executable, and the upgrade lands right
/// after. Nothing re-checks the game (by design for timelocked hooks: capped and blockable), but
/// the notice holders got is zero, not `delay_secs`.
#[test]
fn f5_a_game_is_made_while_an_executable_proposal_of_other_code_is_pending() {
    let mut w = att_world();
    let author = w.env.funded(50 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    w.env.warp(1);
    register(&mut w.env, &author, JACKPOT, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    attest_now(&mut w.env, JACKPOT).ok();
    // New code: half_life's (any other program), proposed and past its eta.
    let new_code = program_bytes("half_life");
    let buffer = Pubkey::new_unique();
    put_buffer(&mut w.env, buffer, timelock_of(&JACKPOT), &new_code, 0);
    w.env
        .send(
            &[tl::propose(
                author.pubkey(),
                JACKPOT,
                buffer,
                trimmed_len(&new_code) as u32,
            )],
            &[&author],
        )
        .ok();
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY));
    // Fixed (round 1): refused while other code is proposed; the author cancels or Studio attests
    // the proposed code. (The upgrade can still land later: holders see the countdown.)
    make_game(&mut w, true).expect_code(ccode(CompanionError::HookNotAttested));
    // The upgrade lands at once (the ProgramData grown first, by anyone).
    let pd_len = w.env.account(&programdata_address(&JACKPOT)).unwrap().data.len() - 45;
    if new_code.len() > pd_len {
        extend(&mut w.env, JACKPOT, ((new_code.len() - pd_len) as u32).max(10_240)).ok();
    }
    let sender = w.env.funded(SOL);
    w.env
        .send(
            &[tl::execute(sender.pubkey(), JACKPOT, buffer, author.pubkey())],
            &[&sender],
        )
        .ok();
    assert_eq!(programdata_hash(&w.env, &JACKPOT), executable_hash(&new_code));
    // The hook now runs other code (half_life's, which refuses the starter's id: its `prepare`
    // fails), under the game made a moment ago; the attestation is stale.
    w.env.warp(1);
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert_ne!(at.build_hash, programdata_hash(&w.env, &JACKPOT));
    make_game(&mut w, true).expect_code(4100);
}

// =============================================================================== F6 (launch)

fn tax_config_args() -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(tax_hook::ID),
        custom_hook_flags: tax_hook::FLAGS,
        label: "Taxed".to_string(),
    }
}

/// F6 (info): a previously failing path changes its error code: an author-upgradeable hook whose
/// authority's own key follows the ProgramData fails `HookTimelockInvalid` (6045-ish) where it
/// failed `HookUpgradeable` before. Old clients (one account) are unchanged.
#[test]
fn f6_an_author_hook_with_its_authority_passed_second_changes_error_code() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let stranger = Keypair::new().pubkey();
    w.env.set_upgrade_authority(tax_hook::ID, Some(stranger));
    let config = Keypair::new();
    let ix = launch::create_config(creator.pubkey(), config.pubkey(), tax_config_args());
    w.env
        .send_paid_by(&[ix], &creator, &[&config])
        .expect_code(u32::from(LaunchError::HookUpgradeable));
    let config = Keypair::new();
    let mut ix = launch::create_config(creator.pubkey(), config.pubkey(), tax_config_args());
    ix.accounts.push(AccountMeta::new_readonly(stranger, false));
    // Fixed (round 1): only an account of hook_timelock's is read as a timelock.
    w.env
        .send_paid_by(&[ix], &creator, &[&config])
        .expect_code(u32::from(LaunchError::HookUpgradeable));
}

// =============================================================================== controls: timelock

const PROGRAM: Pubkey = tax_hook::ID;

struct T {
    env: Env,
    author: Keypair,
}

fn tl_world() -> T {
    let mut env = Env::new();
    let author = env.funded(100 * SOL);
    env.set_upgrade_authority(PROGRAM, Some(author.pubkey()));
    env.warp(1);
    T { env, author }
}

fn registered() -> T {
    let mut t = tl_world();
    let a = t.author.insecure_clone();
    register(&mut t.env, &a, PROGRAM, MIN_DELAY, a.pubkey()).ok();
    t
}

fn lock(env: &Env) -> Timelock {
    env.read::<Timelock>(&timelock_of(&PROGRAM))
}

fn buffer_for_timelock(t: &mut T, code: &[u8]) -> Pubkey {
    let buffer = Pubkey::new_unique();
    put_buffer(&mut t.env, buffer, t.author.pubkey(), code, 0);
    let a = t.author.insecure_clone();
    t.env
        .send(
            &[loader::set_authority(buffer, a.pubkey(), Some(timelock_of(&PROGRAM)))],
            &[&a],
        )
        .ok();
    buffer
}

fn propose(t: &mut T, buffer: Pubkey, code: &[u8]) -> Tx {
    let a = t.author.insecure_clone();
    t.env.send(
        &[tl::propose(a.pubkey(), PROGRAM, buffer, trimmed_len(code) as u32)],
        &[&a],
    )
}

fn raw(accounts: Vec<AccountMeta>, tag: u32) -> Instruction {
    Instruction {
        program_id: BPF_LOADER_UPGRADEABLE_ID,
        accounts,
        data: tag.to_le_bytes().to_vec(),
    }
}

/// The author (or anyone) talking to the loader directly can't touch the program or a proposed
/// buffer once the timelock holds them: no write, no authority change, no close, no upgrade, no
/// close of the program (the 4-account `Close`). The proposal's bytes and hash stay.
#[test]
fn c_timelock_raw_loader_attacks_by_the_author_fail() {
    let mut t = registered();
    let a = t.author.insecure_clone();
    let good = program_bytes("tax_hook");
    let buffer = buffer_for_timelock(&mut t, &good);
    propose(&mut t, buffer, &good).ok();
    let before = t.env.account(&buffer).unwrap();
    let pd = programdata_address(&PROGRAM);
    let attempts: Vec<Instruction> = vec![
        loader::write(buffer, a.pubkey(), 0, vec![0xde, 0xad]),
        loader::set_authority(buffer, a.pubkey(), Some(a.pubkey())),
        loader::close_buffer(buffer, a.pubkey(), a.pubkey()),
        loader::set_authority(pd, a.pubkey(), Some(a.pubkey())),
        loader::set_authority(pd, a.pubkey(), None),
        loader::set_authority_checked(pd, a.pubkey(), a.pubkey()),
        loader::upgrade(pd, PROGRAM, buffer, a.pubkey(), a.pubkey()),
        raw(
            vec![
                AccountMeta::new(pd, false),
                AccountMeta::new(a.pubkey(), false),
                AccountMeta::new_readonly(a.pubkey(), true),
                AccountMeta::new(PROGRAM, false),
            ],
            5,
        ),
    ];
    for ix in attempts {
        t.env.send(&[ix], &[&a]).expect_fail();
        t.env.warp(1);
    }
    assert_eq!(t.env.account(&buffer).unwrap().data, before.data);
    assert_eq!(upgrade_authority(&t.env, &PROGRAM), Some(timelock_of(&PROGRAM)));
    assert_eq!(buffer_authority(&t.env, &buffer), Some(timelock_of(&PROGRAM)));
    // The timelock's own instructions can't be pointed at the ProgramData either.
    t.env
        .send(&[tl::reclaim_buffer(a.pubkey(), PROGRAM, pd)], &[&a])
        .expect_code(tcode(TimelockError::BufferAuthority));
    t.env
        .send(&[tl::cancel(a.pubkey(), PROGRAM, pd)], &[&a])
        .expect_code(tcode(TimelockError::WrongBuffer));
    let s = t.env.funded(SOL);
    t.env
        .send(&[tl::execute(s.pubkey(), PROGRAM, pd, a.pubkey())], &[&s])
        .expect_code(tcode(TimelockError::WrongBuffer));
}

/// A buffer whose authority is the timelock but that was never proposed can't be executed, even
/// after the pending one's eta; a buffer re-created at a cancelled proposal's address is not the
/// proposal (none is pending) until proposed again, with its own hash and a new eta.
#[test]
fn c_timelock_unproposed_and_recreated_buffers_never_execute() {
    let mut t = registered();
    let a = t.author.insecure_clone();
    let good = program_bytes("tax_hook");
    let evil = program_bytes("half_life");
    let pending = buffer_for_timelock(&mut t, &good);
    let stray = buffer_for_timelock(&mut t, &evil);
    propose(&mut t, pending, &good).ok();
    t.env.warp(i64::from(MIN_DELAY));
    let s = t.env.funded(SOL);
    t.env
        .send(&[tl::execute(s.pubkey(), PROGRAM, stray, a.pubkey())], &[&s])
        .expect_code(tcode(TimelockError::WrongBuffer));
    // Cancel, then a buffer of other code at the same address (the author's key re-creates it).
    t.env
        .send(&[tl::cancel(a.pubkey(), PROGRAM, pending)], &[&a])
        .ok();
    put_buffer(&mut t.env, pending, timelock_of(&PROGRAM), &evil, 0);
    t.env.warp(1);
    t.env
        .send(&[tl::execute(s.pubkey(), PROGRAM, pending, a.pubkey())], &[&s])
        .expect_code(tcode(TimelockError::WrongBuffer));
    // Proposed again: its own hash, the whole delay again.
    propose(&mut t, pending, &evil).ok();
    let l = lock(&t.env);
    assert_eq!(l.pending_hash, executable_hash(&evil));
    assert_eq!(l.eta, t.env.now + i64::from(MIN_DELAY));
    t.env
        .send(&[tl::execute(s.pubkey(), PROGRAM, pending, a.pubkey())], &[&s])
        .expect_code(tcode(TimelockError::TooEarly));
}

/// Register: lamports sent to the timelock's address first don't block it; a program whose
/// authority was handed to the timelock's address without `register` (anyone's `SetAuthority`
/// needs no consent of the new key) can't be registered by anyone afterwards and is classed
/// `HookTimelockInvalid` by the launchpad (fail closed: it is frozen, not "timelocked").
#[test]
fn c_timelock_register_prefunded_and_donated_address() {
    let mut t = tl_world();
    let a = t.author.insecure_clone();
    t.env.fund(timelock_of(&PROGRAM), 1_000_000);
    register(&mut t.env, &a, PROGRAM, MIN_DELAY, a.pubkey()).ok();
    assert_eq!(lock(&t.env).author, a.pubkey());

    let mut w = World::new();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(PROGRAM, Some(author.pubkey()));
    w.env.warp(1);
    w.env
        .send(
            &[loader::set_authority(
                programdata_address(&PROGRAM),
                author.pubkey(),
                Some(timelock_of(&PROGRAM)),
            )],
            &[&author],
        )
        .ok();
    register(&mut w.env, &author, PROGRAM, MIN_DELAY, author.pubkey())
        .expect_code(tcode(TimelockError::NotUpgradeable));
    let creator = w.wallet_with_sol(5 * SOL);
    let config = Keypair::new();
    let ix = launch::create_config_timelocked(creator.pubkey(), config.pubkey(), tax_config_args());
    w.env
        .send_paid_by(&[ix], &creator, &[&config])
        .expect_code(u32::from(LaunchError::HookTimelockInvalid));
}

/// A pending author can't act before accepting, and proposing a new pending author (or none)
/// replaces it; the old author keeps full power until the handover is accepted.
#[test]
fn c_timelock_pending_author_games() {
    let mut t = registered();
    let a = t.author.insecure_clone();
    let b = t.env.funded(SOL);
    t.env
        .send(&[tl::propose_author(a.pubkey(), PROGRAM, b.pubkey())], &[&a])
        .ok();
    let good = program_bytes("tax_hook");
    let buf = Pubkey::new_unique();
    put_buffer(&mut t.env, buf, timelock_of(&PROGRAM), &good, 0);
    t.env
        .send(
            &[tl::propose(b.pubkey(), PROGRAM, buf, trimmed_len(&good) as u32)],
            &[&b],
        )
        .expect_code(tcode(TimelockError::NotAuthor));
    t.env
        .send(&[tl::finalize(b.pubkey(), PROGRAM)], &[&b])
        .expect_code(tcode(TimelockError::NotAuthor));
    // Withdrawn: B can no longer accept.
    t.env
        .send(&[tl::propose_author(a.pubkey(), PROGRAM, Pubkey::default())], &[&a])
        .ok();
    t.env
        .send(&[tl::accept_author(b.pubkey(), PROGRAM)], &[&b])
        .expect_code(tcode(TimelockError::NotPendingAuthor));
    assert_eq!(lock(&t.env).author, a.pubkey());
}

// =============================================================================== controls: companion A

/// Vetting reads the hook's own ProgramData and attestation by address: another program's
/// ProgramData, or an attestation account that names another program, vets nothing.
#[test]
fn c_vetting_takes_no_other_programs_accounts() {
    let mut w = att_world();
    attest_now(&mut w.env, JACKPOT).ok();
    // half_life is upgradeable by the protocol's key: its ProgramData in the hook's place.
    let other_pd = programdata_address(&half_life::ID);
    let own_pd = programdata_address(&JACKPOT);
    make_game_with(&mut w, false, |ix| {
        for m in ix.accounts.iter_mut() {
            if m.pubkey == own_pd {
                m.pubkey = other_pd;
            }
        }
    })
    .expect_code(ccode(CompanionError::GameHookNotAccepted));
    // An attestation at the hook's address that names another program: refused.
    let mut a: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    a.program = half_life::ID;
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&a, &mut data).unwrap();
    let mut acc = w.env.account(&companion::attestation_address(&JACKPOT)).unwrap();
    acc.data[..data.len()].copy_from_slice(&data);
    w.env.put(companion::attestation_address(&JACKPOT), acc);
    make_game(&mut w, false).expect_code(ccode(CompanionError::HookNotAttested));
    // Restored, and current again.
    let good = bordrless_program_tests::attest::attestation_of(&w.env, &JACKPOT);
    put_attestation_account(&mut w.env, &good);
    make_game(&mut w, false).ok();
}

/// The fast path (same ProgramData slot) can't be reached by changed code: an upgrade sets the
/// slot to the upgrade's, and the loader refuses a second deploy or extension in that slot.
#[test]
fn c_attestation_slot_cant_be_reused_by_an_upgrade() {
    let mut w = att_world();
    let author = w.env.funded(50 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    w.env.warp(1);
    register(&mut w.env, &author, JACKPOT, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    attest_now(&mut w.env, JACKPOT).ok();
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    // Same code (the starter's own bytes) proposed and executed: the slot moves, the hash is equal.
    let same = program_bytes("studio_jackpot");
    let buffer = Pubkey::new_unique();
    put_buffer(&mut w.env, buffer, timelock_of(&JACKPOT), &same, 0);
    w.env
        .send(
            &[tl::propose(author.pubkey(), JACKPOT, buffer, trimmed_len(&same) as u32)],
            &[&author],
        )
        .ok();
    w.env.warp(i64::from(MIN_ACCEPTED_DELAY));
    let s = w.env.funded(SOL);
    w.env
        .send(&[tl::execute(s.pubkey(), JACKPOT, buffer, author.pubkey())], &[&s])
        .ok();
    assert_ne!(programdata_slot(&w.env, &JACKPOT), at.programdata_slot);
    // In the same slot nobody can extend (or upgrade) again.
    let g = w.env.funded(10 * SOL);
    w.env
        .send(
            &[loader::extend_program(
                programdata_address(&JACKPOT),
                JACKPOT,
                g.pubkey(),
                10_240,
            )],
            &[&g],
        )
        .expect_fail();
    w.env.warp(1);
    // Same code: still current through the hash.
    make_game(&mut w, true).ok();
}

/// `attest` is the attester's only; a program that is not an upgradeable-loader program (or a
/// ProgramData passed as the program) can't be attested; the hash is always the chain's.
#[test]
fn c_attest_only_hashes_what_is_on_chain() {
    let mut w = att_world();
    let a = att_args(&w.env, &JACKPOT);
    // The ProgramData passed as the program.
    let pd = programdata_address(&JACKPOT);
    let mut ix = companion::attest(ATTESTER, JACKPOT, a.clone());
    ix.accounts[1].pubkey = pd;
    send_as(&mut w.env, &[ix], &[], &[ATTESTER]).expect_fail();
    // Another program's ProgramData with this program: refused.
    let mut ix = companion::attest(ATTESTER, JACKPOT, a.clone());
    ix.accounts[2].pubkey = programdata_address(&half_life::ID);
    send_as(&mut w.env, &[ix], &[], &[ATTESTER])
        .expect_code(ccode(CompanionError::ProgramAccounts));
    // half_life's hash for JACKPOT: refused.
    let mut b = a.clone();
    b.build_hash = programdata_hash(&w.env, &half_life::ID);
    attest(&mut w.env, JACKPOT, b).expect_code(ccode(CompanionError::AttestationHashMismatch));
    attest(&mut w.env, JACKPOT, a).ok();
}

/// `set_hook_status_v2` audits only Immutable or Bordrless-managed code, with the code's own hash:
/// a timelocked program (its Timelock left out or passed) and a ProgramData passed as the program
/// are refused.
#[test]
fn c_v2_refuses_timelocked_and_mismatched_accounts() {
    let mut w = att_world();
    let deployer = w.env.deployer.insecure_clone();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    w.env.warp(1);
    register(&mut w.env, &author, JACKPOT, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    let hash = programdata_hash(&w.env, &JACKPOT);
    let v2 = companion::set_hook_status_v2(deployer.pubkey(), JACKPOT, status_args(true), hash);
    w.env
        .send(std::slice::from_ref(&v2), &[&deployer])
        .expect_code(ccode(CompanionError::AuditNeedsFixedCode));
    // Timelock left out: can't be classed at all.
    let mut no_lock = v2.clone();
    no_lock
        .accounts
        .retain(|m| m.pubkey != timelock_of(&JACKPOT));
    w.env
        .send(&[no_lock], &[&deployer])
        .expect_code(ccode(CompanionError::ProgramAccounts));
    // An immutable program's ProgramData and hash offered for this one: it is not found.
    w.env.set_upgrade_authority(half_life::ID, None);
    let hl = programdata_hash(&w.env, &half_life::ID);
    let mut swapped =
        companion::set_hook_status_v2(deployer.pubkey(), JACKPOT, status_args(true), hl);
    for m in swapped.accounts.iter_mut() {
        if m.pubkey == programdata_address(&JACKPOT) {
            m.pubkey = programdata_address(&half_life::ID);
        }
    }
    w.env
        .send(&[swapped], &[&deployer])
        .expect_code(ccode(CompanionError::ProgramAccounts));
}

// =============================================================================== controls: strategy custody

mod strat {
    use super::*;
    use bordrless_companion::events::{CandidateRejected, StrategyPaid};
    use bordrless_companion::instructions::StrategyArgs;
    use bordrless_companion::state::{Companion, DrawStatus, Game};
    use bordrless_core::policy;
    use bordrless_game::round_of;
    use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};
    use bordrless_program_tests::env::compute_unit_limit;
    use bordrless_program_tests::launch::VQ;
    use lottery_hook::client as lottery;
    use solana_account::Account;
    use strategy_tester as st;

    pub const ROUND: u32 = 3_600;
    const R: i64 = ROUND as i64;
    pub const HOOK: Pubkey = lottery_hook::ID;
    pub const STRATEGY: Pubkey = st::ID;

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

    pub struct Strat {
        pub w: World,
        pub mint: Pubkey,
        pub cranker: Keypair,
    }

    impl Strat {
        pub fn new() -> Self {
            let mut w = World::new();
            w.env
                .svm
                .add_program(STRATEGY, &program_bytes("strategy_tester"))
                .expect("load the tester");
            w.env.set_upgrade_authority(STRATEGY, Some(STUDIO_KEY));
            let launcher = w.wallet_with_sol(50 * SOL);
            let mint_kp = Keypair::new();
            let mint = mint_kp.pubkey();
            put_owned(
                &mut w,
                config_address(&mint),
                STRATEGY,
                st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_PRO_RATA, 0),
            );
            let list = vec![ExtraAccount {
                writable: false,
                source: AccountSource::Pda {
                    program: STRATEGY,
                    seeds: vec![Seed::Literal(b"config".to_vec()), Seed::Account(0)],
                },
            }];
            put_owned(
                &mut w,
                bordrless_strategy::registry_address(&STRATEGY, &mint).0,
                STRATEGY,
                HookAccountList::new(list).encode(),
            );
            let g = CreateGameArgs {
                kind: GameKind::Strategy,
                hook: HOOK,
                split: Split {
                    buyback_bps: 3_000,
                    holders_bps: 0,
                    beneficiary_bps: 0,
                },
                pot_bps: 7_000,
                round_secs: ROUND,
                min_pot: 100_000_000,
                prize_bps: 0,
                claim_window_secs: 600,
                max_attempts: 0,
            };
            let s = StrategyArgs {
                strategy: STRATEGY,
                budget_bps: MAX_STRATEGY_BUDGET_BPS,
                max_share_bps: MAX_STRATEGY_SHARE_BPS,
                max_per_tx: MAX_STRATEGY_PER_TX,
                plan_cu_max: MAX_PLAN_CU,
                entitle_cu_max: MAX_ENTITLE_CU,
                min_weight: 1,
            };
            let ixs = vec![
                companion::create(launcher.pubkey(), launcher.pubkey(), mint, super::create_args()),
                lottery::prepare(launcher.pubkey(), mint, ROUND),
                companion::create_strategy_game(
                    launcher.pubkey(),
                    mint,
                    g,
                    s,
                    vec![],
                    false,
                    &[config_address(&mint)],
                ),
            ];
            w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
            let (config, tx) = w.create_config(
                &launcher,
                CreateConfigArgs {
                    rules: LaunchRules::NONE,
                    creator_fee_bps: 200,
                    custom_hook: Some(HOOK),
                    custom_hook_flags: lottery_hook::FLAGS,
                    label: "Strategy".to_string(),
                },
            );
            tx.ok();
            let c = w.launch_config(&config);
            let custom = c.custom_hook.map(|h| w.custom_hook_accounts(&h, &mint));
            let args = World::launch_args("STRAT", c.creator_fee_bps, VQ, c.rules);
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
            Self { w, mint, cranker }
        }

        pub fn round(&self) -> u32 {
            round_of(self.w.env.now, ROUND)
        }

        pub fn warp_into(&mut self, round: u32, secs: i64) {
            let t = i64::from(round) * R + secs;
            self.w.env.warp(t - self.w.env.now);
        }

        pub fn send(&mut self, ix: Instruction) -> Tx {
            let cranker = self.cranker.insecure_clone();
            self.w.env.send_paid_by(&[ix], &cranker, &[])
        }

        pub fn game(&self) -> Game {
            self.w.env.read(&companion::game_address(&self.mint))
        }

        pub fn companion(&self) -> Companion {
            self.w.env.read(&companion::companion_address(&self.mint))
        }

        pub fn buyer(&mut self, sol: u64) -> Keypair {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            t
        }

        pub fn volume(&mut self, sol: u64) {
            let t = self.w.wallet_with_sol(sol + SOL);
            self.w.buy(&t, &self.mint, sol).ok();
            let held = self.w.env.holding(&self.mint, &t.pubkey());
            self.w.sell(&t, &self.mint, held).ok();
        }

        pub fn claim_fees(&mut self) -> Tx {
            let ix = companion::claim_fees_game(self.cranker.pubkey(), self.mint, HOOK);
            self.send(ix)
        }

        pub fn period_with_holders(&mut self) -> (Keypair, Keypair, u32) {
            let a = self.buyer(5 * SOL);
            let b = self.buyer(SOL);
            let p = self.round() + 1;
            self.warp_into(p, 5);
            for h in [&a, &b] {
                let ix = lottery::enter(self.mint, h.pubkey());
                self.send(ix).ok();
            }
            self.volume(20 * SOL);
            self.claim_fees().ok();
            self.warp_into(p + 1, 10);
            (a, b, p)
        }

        pub fn plan(&mut self, period: u32) -> Tx {
            let ix = companion::plan_period(
                self.cranker.pubkey(),
                self.mint,
                HOOK,
                STRATEGY,
                self.w.launch(&self.mint).pool,
                &[config_address(&self.mint)],
                period,
            );
            self.send(ix)
        }

        pub fn pay(&mut self, period: u32, owners: &[Pubkey]) -> Tx {
            let ix = companion::pay_strategy(
                self.cranker.pubkey(),
                self.mint,
                HOOK,
                STRATEGY,
                &[config_address(&self.mint)],
                period,
                owners,
            );
            let cranker = self.cranker.insecure_clone();
            self.w
                .env
                .send_v0(&[compute_unit_limit(1_400_000), ix], &cranker, &[], &[])
        }

        pub fn set_status(&mut self, program: Pubkey, audited: bool, blocked: bool) -> Tx {
            let deployer = self.w.env.deployer.insecure_clone();
            let ix = companion::set_hook_status(
                deployer.pubkey(),
                program,
                HookStatusArgs {
                    audited,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked,
                },
            );
            self.w.env.send_paid_by(&[ix], &deployer, &[])
        }

        pub fn set_flat(&mut self, amount: u64) {
            let mint = self.mint;
            put_owned(
                &mut self.w,
                config_address(&mint),
                STRATEGY,
                st::config_bytes(st::MODE_PRO_RATA, 0, st::MODE_FLAT, amount),
            );
        }
    }

    /// The ticket hook blocked by the protocol while a period is open: the next fee claim (which
    /// reads only the ticket hook's status) sends the pot, locked budget included, to the buyback
    /// and zeroes `pot_locked`, the period still "open". An audit then lifts the block and new fees
    /// refill the pot: the open period pays nothing from the new pot (`total <= pot_locked` fails
    /// the whole payment), and the next period plans normally once it has ended.
    #[test]
    fn c_strategy_block_through_claim_fees_then_unblock_fails_closed() {
        let mut s = Strat::new();
        let (a, b, p) = s.period_with_holders();
        s.plan(p).ok();
        let c0 = s.companion();
        assert!(c0.pot_locked > 0);
        s.set_status(HOOK, false, true).ok();
        s.volume(5 * SOL);
        s.claim_fees().ok();
        let c1 = s.companion();
        assert_eq!((c1.pending_pot, c1.pot_locked), (0, 0));
        assert_eq!(s.game().status, DrawStatus::Revealed);
        // Lifted (by an audit), and refilled.
        s.set_status(HOOK, true, false).ok();
        s.volume(20 * SOL);
        s.claim_fees().ok();
        let c2 = s.companion();
        assert!(c2.pending_pot > 0 && c2.pot_locked == 0);
        let before = s.w.env.lamports(&a.pubkey());
        let tx = s.pay(p, &[a.pubkey(), b.pubkey()]);
        tx.expect_code(ccode(CompanionError::MathOverflow));
        assert_eq!(s.w.env.lamports(&a.pubkey()), before);
        assert!(s
            .w
            .env
            .account(&companion::receipt_address(&s.mint, p, &a.pubkey()))
            .is_none());
        assert_eq!(s.companion().pending_pot, c2.pending_pot);
    }

    /// A strategy that answers the most it may for everyone pays at most `max_share_bps` of the
    /// budget per holder and never more than the budget in all, over any number of transactions;
    /// what the companion holds drops by exactly what was paid.
    #[test]
    fn c_strategy_flat_maximum_answers_stay_within_the_budget() {
        let mut s = Strat::new();
        let mut holders: Vec<Keypair> = (0..6).map(|_| s.buyer(SOL)).collect();
        let p = s.round() + 1;
        s.warp_into(p, 5);
        for h in &holders {
            let ix = lottery::enter(s.mint, h.pubkey());
            s.send(ix).ok();
        }
        s.volume(20 * SOL);
        s.claim_fees().ok();
        s.warp_into(p + 1, 10);
        s.set_flat(u64::MAX / 2);
        s.plan(p).ok();
        let budget = s.game().prize;
        let pot = s.companion().pending_pot;
        let cap = budget * u64::from(MAX_STRATEGY_SHARE_BPS) / 10_000;
        // Answer exactly the cap for each: 4 are paid, then the budget runs out.
        s.set_flat(cap);
        let mut paid = 0u64;
        let mut n_paid = 0;
        let mut rejected = 0;
        for chunk in holders.chunks(3) {
            let owners: Vec<Pubkey> = chunk.iter().map(|k| k.pubkey()).collect();
            let tx = s.pay(p, &owners);
            tx.ok();
            for e in tx.events::<StrategyPaid>() {
                for x in &e.payments {
                    assert!(x.amount + x.bounty <= cap);
                    paid += x.amount + x.bounty;
                    n_paid += 1;
                }
            }
            rejected += tx.events::<CandidateRejected>().len();
        }
        let g = s.game();
        assert_eq!((n_paid, rejected), (4, 2));
        assert!(g.epoch_paid <= budget);
        assert_eq!(g.epoch_paid, paid);
        assert_eq!(s.companion().pending_pot, pot - paid);
        assert_eq!(s.companion().pot_locked, budget - paid);
        holders.clear();
    }
}
