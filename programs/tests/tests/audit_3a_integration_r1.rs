//! Audit 3a (integration, r1): proofs of concept for `log-3a-audit-integration-r1.md`. Each test
//! asserts what the code does today, so a fix flips it.
//!
//! - F-P1: `set_hook_status` (v1), the instruction every old client and the SDK's
//!   `setHookStatusChecked` send, rewrites `HookStatus.reserved`, so it erases the audited hash a
//!   `set_hook_status_v2` audit recorded: the hook stays audited (uncapped, unblockable) on chain,
//!   and every label turns to "audited: stale" although the code never changed.
//! - F-P2: an attestation of any `kind` (0, a token hook) vets a game hook: the companion never
//!   reads `kind`.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::instructions::{
    AttestArgs, CreateArgs, CreateGameArgs, GameKindArgs, HookStatusArgs,
};
use bordrless_companion::state::{GameKind, HookStatus, Split};
use bordrless_hook::authority::{programdata_address, timelock_address};
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::SOL;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::risk::{risk_label_of, RawAccount, RiskAccounts};
use bordrless_program_tests::timelock::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const JACKPOT: Pubkey = studio_jackpot::ID;
const STUDIO_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[0];

fn world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(JACKPOT, &program_bytes("studio_jackpot"))
        .expect("load");
    w.env.set_upgrade_authority(JACKPOT, Some(STUDIO_KEY));
    w.env.without_sigverify();
    w.env.fund(STUDIO_ATTESTER, 10 * SOL);
    w
}

fn attest_kind(env: &mut Env, kind: u8) -> Tx {
    let a = AttestArgs {
        build_hash: programdata_hash(env, &JACKPOT),
        source_hash: [5; 32],
        template_commit: [6; 20],
        sim_version: 3,
        sim_pass: true,
        cut_max_bps: 0,
        cap_bps: 0,
        review: REVIEW_PASS,
        kind,
    };
    send_as(
        env,
        &[companion::attest(STUDIO_ATTESTER, JACKPOT, a)],
        &[],
        &[STUDIO_ATTESTER],
    )
}

fn raw(env: &Env, key: &Pubkey) -> Option<RawAccount> {
    env.account(key).map(|a| RawAccount {
        owner: a.owner,
        data: a.data,
        executable: a.executable,
    })
}

fn label_audited(env: &Env) -> &'static str {
    let accounts = RiskAccounts {
        program_id: JACKPOT,
        program: raw(env, &JACKPOT),
        programdata: raw(env, &programdata_address(&JACKPOT)),
        timelock: raw(env, &timelock_address(&JACKPOT).0),
        timelock_programdata: raw(env, &programdata_address(&hook_timelock::ID)),
        status: raw(env, &companion::hook_status_address(&JACKPOT)),
        attestation: raw(env, &companion::attestation_address(&JACKPOT)),
    };
    risk_label_of(&accounts, env.now, Some(programdata_hash(env, &JACKPOT))).audited
}

#[test]
fn f_p1_a_v1_rewrite_erases_the_audited_hash() {
    let mut w = world();
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &JACKPOT);
    let audited = HookStatusArgs {
        audited: true,
        pot_cap: DEFAULT_POT_CAP,
        blocked: false,
    };
    w.env
        .send(
            &[companion::set_hook_status_v2(deployer.pubkey(), JACKPOT, audited, hash)],
            &[&deployer],
        )
        .ok();
    let s: HookStatus = w.env.read(&companion::hook_status_address(&JACKPOT));
    assert_eq!(s.audited_hash(), hash);
    assert_eq!(label_audited(&w.env), "current");
    // The same audit written again by v1 (as deployed, or as the SDK's `setHookStatusChecked`):
    // still audited on chain, its hash gone.
    w.env.warp(5);
    w.env
        .send(
            &[companion::set_hook_status_checked(deployer.pubkey(), JACKPOT, audited)],
            &[&deployer],
        )
        .ok();
    // Fixed (round 1): v1 keeps the hash an earlier audit recorded.
    let s: HookStatus = w.env.read(&companion::hook_status_address(&JACKPOT));
    assert!(s.audited);
    assert_eq!(s.audited_hash(), hash);
    assert_eq!(programdata_hash(&w.env, &JACKPOT), hash, "the code never changed");
    assert_eq!(label_audited(&w.env), "current");
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

#[test]
fn f_p2_a_token_hook_attestation_vets_a_game_hook() {
    let mut w = world();
    attest_kind(&mut w.env, 0).ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let args = CreateGameArgs {
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
    };
    let k = GameKindArgs {
        timer_secs: studio_jackpot::TIMER_SECS,
        min_tokens: studio_jackpot::MIN_TOKENS,
        ..GameKindArgs::default()
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), mint, create_args()),
        prepare_ix(launcher.pubkey(), mint),
        companion::create_game_v2_attested(launcher.pubkey(), mint, args, k, false),
    ];
    // Fixed (round 1): only a game-hook attestation (kind 1) vets a game hook.
    w.env
        .send_paid_by(&ixs, &launcher, &[&mint_kp])
        .expect_code(u32::from(
            bordrless_companion::error::CompanionError::HookNotAttested,
        ));
}
