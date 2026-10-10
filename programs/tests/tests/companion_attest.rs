//! Phase 3a (`docs/phase3a.md` §2.3, §6, owner decision 7): Bordrless Studio's attestations, the
//! game hooks the companion takes without a status, and audits tied to code.
//!
//! The attester's key is a mainnet hot key the suites never hold: `attest` is sent with sigverify
//! off and the attester's signature left empty (`send_as`), which is all the program can check (the
//! signer flag and the address).

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{
    AttestArgs, CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs,
};
use bordrless_companion::state::{GameKind, HookAttestation, HookStatus, Split};
use bordrless_hook::authority::{programdata_address, MIN_ACCEPTED_DELAY};
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::SOL;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::*;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const JACKPOT: Pubkey = studio_jackpot::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];
const ATTESTER: Pubkey = STUDIO_ATTESTER;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
}

fn world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(JACKPOT, &program_bytes("studio_jackpot"))
        .expect("load");
    w.env.set_upgrade_authority(JACKPOT, Some(STUDIO_KEY));
    w.env.without_sigverify();
    w.env.fund(ATTESTER, 10 * SOL);
    w
}

fn args(env: &Env, program: &Pubkey) -> AttestArgs {
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

/// `create_game_v2` of a jackpot on the starter, with the vetting accounts the client passes.
fn make_game(w: &mut World, timelocked: bool) -> Tx {
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (a, k) = jackpot_args();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(launcher.pubkey(), mint),
        companion::create_game_v2_attested(launcher.pubkey(), mint, a, k, timelocked),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp])
}

// =============================================================================== attest, revoke

#[test]
fn the_attester_attests_the_code_on_chain() {
    let mut w = world();
    let a = args(&w.env, &JACKPOT);
    let tx = attest(&mut w.env, JACKPOT, a.clone());
    tx.ok();
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert_eq!(
        (at.program, at.build_hash, at.attester, at.revoked),
        (JACKPOT, a.build_hash, ATTESTER, false)
    );
    assert_eq!(at.programdata_slot, programdata_slot(&w.env, &JACKPOT));
    assert_eq!((at.template_commit, at.sim_version), ([6; 20], 3));
    println!("attest: {} CU, {} bytes", tx.cu(), tx.size);
    // A hash that is not the code's: refused.
    let mut bad = a.clone();
    bad.build_hash[0] ^= 1;
    refused(
        &attest(&mut w.env, JACKPOT, bad),
        CompanionError::AttestationHashMismatch,
    );
    // A failing simulation or an unknown review gets no attestation.
    let mut fail = a.clone();
    fail.sim_pass = false;
    refused(&attest(&mut w.env, JACKPOT, fail), CompanionError::BadAttestation);
    let mut odd = a.clone();
    odd.review = 2;
    refused(&attest(&mut w.env, JACKPOT, odd), CompanionError::BadAttestation);
    // Anyone else: refused (Studio's upgrade key included).
    let stranger = w.env.funded(SOL);
    let mut ix = companion::attest(stranger.pubkey(), JACKPOT, a.clone());
    ix.accounts[0] = AccountMeta::new(stranger.pubkey(), true);
    refused(
        &w.env.send(&[ix], &[&stranger]),
        CompanionError::NotAttester,
    );
    let ix = companion::attest(STUDIO_KEY, JACKPOT, a.clone());
    w.env.fund(STUDIO_KEY, SOL);
    refused(
        &send_as(&mut w.env, &[ix], &[], &[STUDIO_KEY]),
        CompanionError::NotAttester,
    );
    // A re-attestation (a new build) rewrites it.
    w.env.warp(10);
    attest(&mut w.env, JACKPOT, a).ok();
    let again: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert_eq!(again.attested_at, w.env.now);
}

#[test]
fn only_the_attester_or_the_protocol_revokes() {
    let mut w = world();
    let a = args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a).ok();
    let stranger = w.env.funded(SOL);
    refused(
        &w.env
            .send(&[companion::revoke(stranger.pubkey(), JACKPOT)], &[&stranger]),
        CompanionError::NotAttester,
    );
    let deployer = w.env.deployer.insecure_clone();
    let tx = w
        .env
        .send(&[companion::revoke(deployer.pubkey(), JACKPOT)], &[&deployer]);
    tx.ok();
    assert_eq!(tx.event::<AttestationRevoked>().by, deployer.pubkey());
    let at: HookAttestation = w.env.read(&companion::attestation_address(&JACKPOT));
    assert!(at.revoked && at.revoked_at == w.env.now);
    // The attester too (idempotent).
    send_as(
        &mut w.env,
        &[companion::revoke(ATTESTER, JACKPOT)],
        &[],
        &[ATTESTER],
    )
    .ok();
}

// =============================================================================== game hooks

#[test]
fn a_studio_hook_needs_a_current_attestation() {
    let mut w = world();
    // No attestation: refused, though Studio's key upgrades it (the donation gap, closed).
    refused(&make_game(&mut w, false), CompanionError::HookNotAttested);
    let a = args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a.clone()).ok();
    make_game(&mut w, false).ok();
    // An extension (anyone may send one) changes the slot, not the code: still current.
    let payer = w.env.funded(10 * SOL);
    w.env.warp(1);
    w.env
        .send(
            &[hook_timelock::loader::extend_program(
                programdata_address(&JACKPOT),
                JACKPOT,
                payer.pubkey(),
                10_240,
            )],
            &[&payer],
        )
        .ok();
    assert_ne!(programdata_slot(&w.env, &JACKPOT), a_slot(&w.env));
    w.env.warp(1);
    make_game(&mut w, false).ok();
    // Revoked: refused.
    send_as(
        &mut w.env,
        &[companion::revoke(ATTESTER, JACKPOT)],
        &[],
        &[ATTESTER],
    )
    .ok();
    refused(&make_game(&mut w, false), CompanionError::HookNotAttested);
    // Re-attested, then the code changes (Studio upgrades it): stale, refused.
    attest(&mut w.env, JACKPOT, a).ok();
    make_game(&mut w, false).ok();
    let pd = programdata_address(&JACKPOT);
    let mut acc = w.env.account(&pd).unwrap();
    let last = acc.data.len() - 20_000;
    acc.data[last] ^= 0x5a;
    // (A raw write of the ProgramData as an upgrade leaves it, its slot moved, without reloading
    // the program.)
    let slot = programdata_slot(&w.env, &JACKPOT) + 7;
    bordrless_program_tests::env::write_programdata_header(&mut acc.data, slot, Some(STUDIO_KEY));
    w.env.svm.set_account(pd, acc).ok();
    refused(&make_game(&mut w, false), CompanionError::HookNotAttested);
}

fn a_slot(env: &Env) -> u64 {
    let at: HookAttestation = env.read(&companion::attestation_address(&JACKPOT));
    at.programdata_slot
}

#[test]
fn a_timelocked_game_hook_needs_its_timelock_and_an_attestation() {
    let mut w = world();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    // An author's key: not vetted at all.
    refused(&make_game(&mut w, false), CompanionError::GameHookNotAccepted);
    register(&mut w.env, &author, JACKPOT, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    // Timelocked, without its Timelock passed: can't be classed, not vetted.
    refused(&make_game(&mut w, false), CompanionError::GameHookNotAccepted);
    // With it, but no attestation.
    refused(&make_game(&mut w, true), CompanionError::HookNotAttested);
    let a = args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a).ok();
    let tx = make_game(&mut w, true);
    tx.ok();
    let set: GameKindSet = tx.event();
    assert_eq!((set.audited, set.pot_cap), (false, DEFAULT_POT_CAP));
}

#[test]
fn bordrless_lottery_hook_is_taken_as_deployed_and_a_status_still_vets() {
    // lottery_hook needs no attestation (taken by id, as phase 1 deployed it): companion_game.rs
    // runs it. A hook with a status needs none either.
    let mut w = world();
    let deployer = w.env.deployer.insecure_clone();
    w.env
        .send(
            &[companion::set_hook_status(
                deployer.pubkey(),
                JACKPOT,
                HookStatusArgs {
                    audited: false,
                    pot_cap: DEFAULT_POT_CAP,
                    blocked: false,
                },
            )],
            &[&deployer],
        )
        .ok();
    make_game(&mut w, false).ok();
}

// =============================================================================== audits tied to code

fn status_args(audited: bool) -> HookStatusArgs {
    HookStatusArgs {
        audited,
        pot_cap: DEFAULT_POT_CAP,
        blocked: false,
    }
}

#[test]
fn an_audit_is_recorded_with_the_codes_hash() {
    let mut w = world();
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &JACKPOT);
    // A wrong hash: refused.
    let mut wrong = hash;
    wrong[31] ^= 1;
    refused(
        &w.env.send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                JACKPOT,
                status_args(true),
                wrong,
            )],
            &[&deployer],
        ),
        CompanionError::AuditHashMismatch,
    );
    // Without an audit, the hash must be zeros.
    refused(
        &w.env.send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                JACKPOT,
                status_args(false),
                hash,
            )],
            &[&deployer],
        ),
        CompanionError::BadHookStatus,
    );
    // The code's hash, of a program Bordrless manages: recorded.
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
    println!("set_hook_status_v2 (audit): {} CU", tx.cu());
    let s: HookStatus = w.env.read(&companion::hook_status_address(&JACKPOT));
    assert!(s.audited);
    assert_eq!(s.audited_hash(), hash);
    assert_eq!(tx.event::<HookAuditRecorded>().audited_hash, hash);
    // Only the protocol's upgrade authority.
    let stranger = w.env.funded(SOL);
    refused(
        &w.env.send(
            &[companion::set_hook_status_v2(
                stranger.pubkey(),
                JACKPOT,
                status_args(true),
                hash,
            )],
            &[&stranger],
        ),
        CompanionError::NotProtocolAuthority,
    );
    // Immutable code can be audited too.
    w.env.set_upgrade_authority(half_life::ID, None);
    let hl = programdata_hash(&w.env, &half_life::ID);
    w.env
        .send(
            &[companion::set_hook_status_v2(
                deployer.pubkey(),
                half_life::ID,
                status_args(true),
                hl,
            )],
            &[&deployer],
        )
        .ok();
}

#[test]
fn a_timelocked_or_author_upgradeable_program_cant_be_audited() {
    let mut w = world();
    let deployer = w.env.deployer.insecure_clone();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    let hash = programdata_hash(&w.env, &JACKPOT);
    let v2 = |hash| {
        companion::set_hook_status_v2(deployer.pubkey(), JACKPOT, status_args(true), hash)
    };
    refused(
        &w.env.send(&[v2(hash)], &[&deployer]),
        CompanionError::AuditNeedsFixedCode,
    );
    register(&mut w.env, &author, JACKPOT, MIN_ACCEPTED_DELAY, author.pubkey()).ok();
    refused(
        &w.env.send(&[v2(hash)], &[&deployer]),
        CompanionError::AuditNeedsFixedCode,
    );
    // v1 with the hook's accounts (as the SDK sends it): refused too.
    refused(
        &w.env.send(
            &[companion::set_hook_status_checked(
                deployer.pubkey(),
                JACKPOT,
                status_args(true),
            )],
            &[&deployer],
        ),
        CompanionError::AuditNeedsFixedCode,
    );
    // v1 as deployed (no accounts): the old behaviour (no hash recorded: labels show it stale).
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
    assert!(s.audited);
    assert_eq!(s.audited_hash(), [0; 32]);
    // Finalized through the timelock, it can be audited with v2.
    w.env
        .send(&[hook_timelock::client::finalize(author.pubkey(), JACKPOT)], &[&author])
        .ok();
    w.env.send(&[v2(hash)], &[&deployer]).ok();
}

#[test]
fn v1_without_an_audit_is_unchanged() {
    let mut w = world();
    let deployer = w.env.deployer.insecure_clone();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    // Caps and blocks of any hook, with or without its accounts.
    for ix in [
        companion::set_hook_status(deployer.pubkey(), JACKPOT, status_args(false)),
        companion::set_hook_status_checked(deployer.pubkey(), JACKPOT, status_args(false)),
    ] {
        w.env.send(&[ix], &[&deployer]).ok();
    }
    let _ = token::holding_address(&JACKPOT, &JACKPOT);
}

/// Independent audit X1, through the companion: a jackpot coin on a timelocked, attested Studio
/// hook. Its config records the timelocked hook; the companion's launch forwards the hook's
/// `Timelock` (last) to `create_launch`, which refuses it while the timelock holds a proposal
/// (staged after the game and the config were made) and takes it once that proposal is cancelled.
#[test]
fn a_timelocked_game_hooks_coin_launches_only_while_nothing_is_staged() {
    use bordrless_launch::client as launch;
    use bordrless_launch::instructions::CreateConfigArgs;
    use bordrless_launch::state::LaunchRules;
    use bordrless_program_tests::launch::VQ;
    use hook_timelock::client as tl;
    let mut w = world();
    let author = w.env.funded(10 * SOL);
    w.env.set_upgrade_authority(JACKPOT, Some(author.pubkey()));
    register(
        &mut w.env,
        &author,
        JACKPOT,
        MIN_ACCEPTED_DELAY,
        author.pubkey(),
    )
    .ok();
    let a = args(&w.env, &JACKPOT);
    attest(&mut w.env, JACKPOT, a).ok();
    // The game (setup) and the config, with nothing staged.
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (ga, k) = jackpot_args();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(launcher.pubkey(), mint),
        companion::create_game_v2_attested(launcher.pubkey(), mint, ga, k, true),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint_kp]).ok();
    let config = Keypair::new();
    let ix = launch::create_config_timelocked(
        launcher.pubkey(),
        config.pubkey(),
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 200,
            custom_hook: Some(JACKPOT),
            custom_hook_flags: GameKind::Jackpot.hook_flags(),
            label: "Game".to_string(),
        },
    );
    w.env.send_paid_by(&[ix], &launcher, &[&config]).ok();
    let c = w.launch_config(&config.pubkey());
    assert!(c.hook_timelocked());
    let launch_ix = |w: &World| {
        let custom = w.custom_hook_accounts(&JACKPOT, &mint);
        let mut args = World::launch_args("JACK", c.creator_fee_bps, VQ, c.rules);
        args.name = "Jackpot".to_string();
        let inner = launch::with_hook_timelock(
            launch::create_launch_with(
                companion::creator_address(&mint),
                mint,
                w.env.treasury.pubkey(),
                w.sol,
                bordrless_core::policy::LP_FEE_BPS,
                args.clone(),
                Some(config.pubkey()),
                Some(&custom),
            ),
            &JACKPOT,
        );
        companion::launch(launcher.pubkey(), mint, &inner, args)
    };
    // The author stages other code after the game and the config: no launch.
    let buffer = Pubkey::new_unique();
    let code = program_bytes("studio_streak");
    put_buffer(&mut w.env, buffer, timelock_of(&JACKPOT), &code, 0);
    w.env
        .send(
            &[tl::propose(
                author.pubkey(),
                JACKPOT,
                buffer,
                bordrless_hook::authority::trimmed_len(&code) as u32,
            )],
            &[&author],
        )
        .ok();
    w.env
        .send_paid_by(&[launch_ix(&w)], &launcher, &[&mint_kp])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::HookTimelockPending,
        ));
    // Cancelled: the coin launches through its companion.
    w.env
        .send(&[tl::cancel(author.pubkey(), JACKPOT, buffer)], &[&author])
        .ok();
    w.env.svm.expire_blockhash();
    w.env
        .send_paid_by(&[launch_ix(&w)], &launcher, &[&mint_kp])
        .ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(JACKPOT));
}
