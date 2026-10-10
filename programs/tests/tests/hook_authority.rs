//! Who may upgrade a custom hook (§5.8): `create_config` takes the hook's program data and refuses a
//! hook that anyone but Bordrless could upgrade, so a token's hook can never be swapped for other
//! code by a stranger after it launched. Allowed: immutable (no upgrade authority), Studio's upgrade
//! key, the protocol's own upgrade authority. Once a config passed, only those keys (or no one) can
//! change who upgrades the hook, so a launch from it takes no program data.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::InstructionData;
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client as launch;
use bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES;
use bordrless_launch::error::LaunchError;
use bordrless_launch::instructions::{programdata_address, CreateConfigArgs};
use bordrless_launch::state::{LaunchConfig, LaunchRules};
use bordrless_program_tests::env::SYSTEM_PROGRAM_ID;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn args() -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(tax_hook::ID),
        custom_hook_flags: tax_hook::FLAGS,
        label: "Taxed".to_string(),
    }
}

/// Rewrites tax_hook's program data so that `authority` may upgrade it (None: immutable).
fn set_authority(w: &mut World, authority: Option<Pubkey>) {
    let key = programdata_address(&tax_hook::ID);
    let mut account = w.env.account(&key).expect("tax_hook's program data");
    assert_eq!(account.data[..4], 3u32.to_le_bytes());
    match authority {
        None => account.data[12] = 0,
        Some(a) => {
            account.data[12] = 1;
            account.data[13..45].copy_from_slice(a.as_ref());
        }
    }
    w.env.put(key, account);
}

/// tax_hook prepared for `mint`: 1% of transfers to `collector`.
fn prepare(w: &mut World, payer: &Keypair, mint: Pubkey, collector: Pubkey) {
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, mint.as_ref()], &tax_hook::ID).0;
    let ix = Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax, false),
            AccountMeta::new(hook_accounts_address(&tax_hook::ID, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: tax_hook::instruction::Prepare {
            fee_bps: 100,
            max_wallet_bps: 0,
        }
        .data(),
    };
    w.env.send_paid_by(&[ix], payer, &[]).ok();
}

fn config_ix(creator: &Keypair, config: &Keypair) -> Instruction {
    launch::create_config(creator.pubkey(), config.pubkey(), args())
}

#[test]
fn a_hook_a_stranger_can_upgrade_is_refused_for_a_config() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    set_authority(&mut w, Some(Keypair::new().pubkey()));
    let config = Keypair::new();
    let tx = w
        .env
        .send_paid_by(&[config_ix(&creator, &config)], &creator, &[&config]);
    tx.expect_code(u32::from(LaunchError::HookUpgradeable));

    // Studio's upgrade key and the protocol's are allowed, and so is no authority at all.
    for authority in [
        Some(HOOK_UPGRADE_AUTHORITIES[0]),
        Some(HOOK_UPGRADE_AUTHORITIES[1]),
        None,
    ] {
        set_authority(&mut w, authority);
        let config = Keypair::new();
        let tx = w
            .env
            .send_paid_by(&[config_ix(&creator, &config)], &creator, &[&config]);
        tx.ok();
    }
}

#[test]
fn the_hooks_program_data_must_be_passed_and_be_its_own() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let config = Keypair::new();
    let mut missing = config_ix(&creator, &config);
    assert_eq!(
        missing.accounts.last().unwrap().pubkey,
        programdata_address(&tax_hook::ID)
    );
    missing.accounts.pop();
    let tx = w.env.send_paid_by(&[missing], &creator, &[&config]);
    tx.expect_code(u32::from(LaunchError::HookProgramDataMissing));
    let config = Keypair::new();
    let mut wrong = config_ix(&creator, &config);
    wrong.accounts.last_mut().unwrap().pubkey = programdata_address(&Keypair::new().pubkey());
    let tx = w.env.send_paid_by(&[wrong], &creator, &[&config]);
    tx.expect_code(u32::from(LaunchError::HookProgramDataMissing));
}

#[test]
fn a_launch_from_an_allowed_config_needs_no_program_data() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    let collector = Keypair::new().pubkey();
    set_authority(&mut w, Some(HOOK_UPGRADE_AUTHORITIES[0]));
    let (config, tx) = w.create_config(&creator, args());
    tx.ok();
    let mint = Keypair::new();
    prepare(&mut w, &creator, mint.pubkey(), collector);
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint.pubkey(), "UPG", VQ, &config);
    assert!(!ix
        .accounts
        .iter()
        .any(|m| m.pubkey == programdata_address(&tax_hook::ID)));
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint]);
    tx.ok();
    assert_eq!(w.launch(&mint.pubkey()).custom_hook, Some(tax_hook::ID));
}

// ---- Phase 3a: a hook behind `hook_timelock` (`docs/phase3a.md` §2, §3.6) ----------------------

use bordrless_hook::authority::{timelock_offsets, MIN_ACCEPTED_DELAY};
use bordrless_program_tests::timelock::{register, timelock_of, upgrade_authority};

/// tax_hook upgradeable by a fresh author, then registered under a timelock of `delay`.
fn timelocked(w: &mut World, delay: u32) -> Keypair {
    let author = w.env.funded(5 * SOL);
    set_authority(w, Some(author.pubkey()));
    register(&mut w.env, &author, tax_hook::ID, delay, author.pubkey()).ok();
    assert_eq!(
        upgrade_authority(&w.env, &tax_hook::ID),
        Some(timelock_of(&tax_hook::ID))
    );
    author
}

fn timelocked_config_ix(creator: &Keypair, config: &Keypair) -> Instruction {
    launch::create_config_timelocked(creator.pubkey(), config.pubkey(), args())
}

#[test]
fn a_timelocked_hook_is_taken_with_its_timelock() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    timelocked(&mut w, MIN_ACCEPTED_DELAY);
    let config = Keypair::new();
    let ix = timelocked_config_ix(&creator, &config);
    assert_eq!(
        ix.accounts.last().unwrap().pubkey,
        timelock_of(&tax_hook::ID)
    );
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    // The config records that its hook is timelocked (X1): every launch from it passes the
    // hook's `Timelock` last (no program data: the class can only stay timelocked or become
    // immutable), so the launch sees whether other code is staged.
    let c = w.launch_config(&config.pubkey());
    assert!(c.hook_timelocked());
    assert_eq!(c.reserved[0], LaunchConfig::TIMELOCKED_HOOK);
    let collector = Keypair::new().pubkey();
    let mint = Keypair::new();
    prepare(&mut w, &creator, mint.pubkey(), collector);
    let ix = w.create_launch_from_config_ix(
        &creator.pubkey(),
        &mint.pubkey(),
        "TLK",
        VQ,
        &config.pubkey(),
    );
    assert_eq!(
        ix.accounts.last().unwrap().pubkey,
        timelock_of(&tax_hook::ID)
    );
    assert!(!ix
        .accounts
        .iter()
        .any(|m| m.pubkey == programdata_address(&tax_hook::ID)));
    // Without it (an old client's accounts): refused.
    let mut bare = ix.clone();
    bare.accounts.pop();
    w.env
        .send_paid_by(&[bare], &creator, &[&mint])
        .expect_code(u32::from(LaunchError::HookTimelockInvalid));
    w.env.svm.expire_blockhash();
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    assert_eq!(w.launch(&mint.pubkey()).custom_hook, Some(tax_hook::ID));
}

#[test]
fn a_timelocked_hook_without_its_timelock_or_with_a_forged_one_is_refused() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    timelocked(&mut w, MIN_ACCEPTED_DELAY);
    let invalid = u32::from(LaunchError::HookTimelockInvalid);
    // The old client (program data only): refused, as a hook nobody vetted.
    let config = Keypair::new();
    w.env
        .send_paid_by(&[config_ix(&creator, &config)], &creator, &[&config])
        .expect_code(invalid);
    // Some other account in the timelock's place.
    let lock = timelock_of(&tax_hook::ID);
    let real = w.env.account(&lock).unwrap();
    let forge = |w: &mut World, data: Vec<u8>, owner: Pubkey| {
        let mut acc = real.clone();
        acc.data = data;
        acc.owner = owner;
        w.env.put(lock, acc);
    };
    let try_config = |w: &mut World| {
        let config = Keypair::new();
        w.env.send_paid_by(
            &[timelocked_config_ix(&creator, &config)],
            &creator,
            &[&config],
        )
    };
    // Wrong owner.
    forge(&mut w, real.data.clone(), Keypair::new().pubkey());
    try_config(&mut w).expect_code(invalid);
    // Discriminator off.
    let mut d = real.data.clone();
    d[0] ^= 1;
    forge(&mut w, d, hook_timelock::ID);
    try_config(&mut w).expect_code(invalid);
    // A timelock for another program.
    let mut d = real.data.clone();
    d[timelock_offsets::PROGRAM..timelock_offsets::PROGRAM + 32]
        .copy_from_slice(half_life::ID.as_ref());
    forge(&mut w, d, hook_timelock::ID);
    try_config(&mut w).expect_code(invalid);
    // Its program data swapped.
    let mut d = real.data.clone();
    d[timelock_offsets::PROGRAMDATA..timelock_offsets::PROGRAMDATA + 32]
        .copy_from_slice(programdata_address(&half_life::ID).as_ref());
    forge(&mut w, d, hook_timelock::ID);
    try_config(&mut w).expect_code(invalid);
    // A delay below the floor (written by no real timelock).
    let mut d = real.data.clone();
    d[timelock_offsets::DELAY_SECS..timelock_offsets::DELAY_SECS + 4]
        .copy_from_slice(&(MIN_ACCEPTED_DELAY - 1).to_le_bytes());
    forge(&mut w, d, hook_timelock::ID);
    try_config(&mut w).expect_code(invalid);
    // A wrong bump.
    let mut d = real.data.clone();
    d[timelock_offsets::BUMP] = d[timelock_offsets::BUMP].wrapping_sub(1);
    forge(&mut w, d, hook_timelock::ID);
    try_config(&mut w).expect_code(invalid);
    // Another program's timelock passed for this one.
    forge(&mut w, real.data.clone(), hook_timelock::ID);
    let half_author = w.env.funded(5 * SOL);
    w.env
        .set_upgrade_authority(half_life::ID, Some(half_author.pubkey()));
    register(
        &mut w.env,
        &half_author,
        half_life::ID,
        MIN_ACCEPTED_DELAY,
        half_author.pubkey(),
    )
    .ok();
    let config = Keypair::new();
    let mut ix = config_ix(&creator, &config);
    ix.accounts.push(AccountMeta::new_readonly(
        timelock_of(&half_life::ID),
        false,
    ));
    w.env
        .send_paid_by(&[ix], &creator, &[&config])
        .expect_code(invalid);
    // The real one passes.
    try_config(&mut w).ok();
}

#[test]
fn old_clients_behave_exactly_as_before() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    let stranger = Keypair::new().pubkey();
    // A stranger's hook: HookUpgradeable, whatever follows the program data (a timelock of
    // another program included).
    set_authority(&mut w, Some(stranger));
    let some_lock = Keypair::new().pubkey();
    for extra in [None, Some(some_lock), Some(timelock_of(&tax_hook::ID))] {
        let config = Keypair::new();
        let mut ix = config_ix(&creator, &config);
        if let Some(e) = extra {
            ix.accounts.push(AccountMeta::new_readonly(e, false));
        }
        w.env
            .send_paid_by(&[ix], &creator, &[&config])
            .expect_code(u32::from(LaunchError::HookUpgradeable));
    }
    // Immutable, Studio's and the protocol's keys: as before, with or without an extra account.
    for authority in [
        None,
        Some(HOOK_UPGRADE_AUTHORITIES[0]),
        Some(HOOK_UPGRADE_AUTHORITIES[1]),
    ] {
        set_authority(&mut w, authority);
        for extra in [false, true] {
            let config = Keypair::new();
            let mut ix = config_ix(&creator, &config);
            if extra {
                ix.accounts
                    .push(AccountMeta::new_readonly(Keypair::new().pubkey(), false));
            }
            w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
        }
    }
    // The instruction an old client builds is unchanged by the new builder for no timelock.
    let config = Keypair::new();
    let old = config_ix(&creator, &config);
    let new = timelocked_config_ix(&creator, &config);
    assert_eq!(old.data, new.data);
    assert_eq!(old.accounts[..], new.accounts[..old.accounts.len()]);
    assert_eq!(new.accounts.len(), old.accounts.len() + 1);
}

#[test]
fn a_loader_v4_hook_classes_by_its_authority() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(30 * SOL);
    // tax_hook rewritten as a loader-v4 program (the bytes check_hook_authority reads).
    let v4 = Pubkey::from_str_const("LoaderV411111111111111111111111111111111111");
    let original = w.env.account(&tax_hook::ID).unwrap();
    for (status, key, ok) in [
        (2u64, Keypair::new().pubkey(), true),
        (0, HOOK_UPGRADE_AUTHORITIES[0], true),
        (0, Keypair::new().pubkey(), false),
        (0, timelock_of(&tax_hook::ID), false),
    ] {
        let mut acc = original.clone();
        // The loader-v4 header, then the code (so the runtime takes the account as a program).
        let mut data = vec![0u8; 48];
        data[8..40].copy_from_slice(key.as_ref());
        data[40..48].copy_from_slice(&status.to_le_bytes());
        data.extend(bordrless_program_tests::program_bytes("tax_hook"));
        acc.data = data;
        acc.owner = v4;
        w.env.put(tax_hook::ID, acc);
        let config = Keypair::new();
        let tx = w.env.send_paid_by(
            &[timelocked_config_ix(&creator, &config)],
            &creator,
            &[&config],
        );
        if ok {
            tx.ok();
        } else {
            tx.expect_code(u32::from(LaunchError::HookUpgradeable));
        }
    }
}
