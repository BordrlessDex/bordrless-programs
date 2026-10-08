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
use bordrless_launch::state::LaunchRules;
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
