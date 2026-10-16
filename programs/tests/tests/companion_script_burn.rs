//! A script burns what it bought (2026-10-16): the keeper's `mintWritable` flag passes the coin's
//! mint writable in a step's remaining accounts (flipping it in the swap's accounts when they are
//! there, else appending it), so a script can buy with the pot and then burn from the creator's
//! holding with the token program's `burn`, the creator address signing. The companion passes its
//! own named mint read-only; the runtime merges a duplicate account's privileges, so the remaining
//! entry is what makes the mint writable for the script.
//!
//! The script is `fixtures/scripts/example` in `MODE_BUY_BURN` / `MODE_BURN`.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs};
use bordrless_companion::state::{Companion, Split};
use bordrless_core::policy;
use bordrless_hook::hook_accounts_address;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Tx, SYSTEM_PROGRAM_ID};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use script_example as se;
use solana_keypair::Keypair;
use solana_signer::Signer;

const SCRIPT: Pubkey = se::ID;
const CREATOR_FEE: u16 = 200;
const BUY: u64 = SOL / 20;
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(u32::from(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

/// A world with the example script loaded and approved, and native launches `native`.
fn world(native: bool) -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(SCRIPT, &program_bytes("script_example"))
        .expect("load the script");
    w.env.set_upgrade_authority(SCRIPT, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = bordrless_program_tests::timelock::programdata_hash(&w.env, &SCRIPT);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                SCRIPT,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
    let ad = w.admin().pubkey();
    w.as_admin(launch::set_native_launches(ad, native)).ok();
    w
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn terms() -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: SCRIPT,
        interval: 60,
        state_len: 64,
        init_data: vec![se::MODE_NOTHING],
    }
}

/// tax_hook prepared for `mint`: 1% of transfers to `collector`.
fn prepare_tax(w: &mut World, payer: &Keypair, mint: Pubkey, collector: Pubkey) {
    use anchor_lang::InstructionData;
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

/// A script companion made and launched (no kit rules, no burn rate); with `taxed`, from a config
/// naming tax_hook as its custom hook.
fn launched(w: &mut World, launcher: &Keypair, taxed: bool) -> Pubkey {
    let mint = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_script(launcher.pubkey(), mint.pubkey(), terms(), vec![]),
    ];
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    let creator = companion::creator_address(&mint.pubkey());
    let (config, custom) = if taxed {
        let (config, tx) = w.create_config(
            launcher,
            CreateConfigArgs {
                rules: LaunchRules::NONE,
                creator_fee_bps: CREATOR_FEE,
                custom_hook: Some(tax_hook::ID),
                custom_hook_flags: tax_hook::FLAGS,
                label: "Taxed".to_string(),
            },
        );
        tx.ok();
        let collector = Keypair::new().pubkey();
        prepare_tax(w, launcher, mint.pubkey(), collector);
        (
            Some(config),
            Some(w.custom_hook_accounts(&tax_hook::ID, &mint.pubkey())),
        )
    } else {
        (None, None)
    };
    let launch_args = World::launch_args("BURN", CREATOR_FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        config,
        custom.as_ref(),
    );
    let ix = companion::launch_script(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    w.env.send_paid_by(&[ix], launcher, &[&mint]).ok();
    mint.pubkey()
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn supply(w: &World, mint: &Pubkey) -> u64 {
    w.env.read::<Mint>(mint).supply
}

/// A wallet's buy and sell on the launch, native (`swap_v2` from its lamports) or bridged.
fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    let native = w.launch(mint).is_native();
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        if native {
            let k = keys(w, mint);
            let tp = t.pubkey();
            let slice = w.launch_base_slice(mint, &tp, &tp, true);
            let buy = launch::swap_v2_with_base_slice(&k, tp, tp, 1, sol, 0, slice);
            w.env
                .send_paid_by(&[token::create_holding(tp, *mint, tp), buy], &t, &[])
                .ok();
            let held = w.env.holding(mint, &tp);
            let slice = w.launch_base_slice(mint, &tp, &tp, false);
            let sell = launch::swap_v2_with_base_slice(&k, tp, tp, 0, held / 4, 0, slice);
            w.env.send_paid_by(&[sell], &t, &[]).ok();
        } else {
            w.buy(&t, mint, sol).ok();
            let held = w.env.holding(mint, &t.pubkey());
            w.sell(&t, mint, held / 4).ok();
        }
    }
}

/// Fees into the pot; answers a cranker.
fn fill_pot(w: &mut World, mint: &Pubkey) -> Keypair {
    w.env.warp(31);
    trade(w, mint, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    assert!(funded.to_pot > BUY + SCRIPT_BOUNTY, "{}", funded.to_pot);
    w.env.warp(30);
    cranker
}

/// `ix`'s accounts and program as remaining accounts, nobody a signer.
fn metas(ix: &Instruction) -> Vec<AccountMeta> {
    let mut m: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    m.push(AccountMeta::new_readonly(ix.program_id, false));
    m
}

/// The keeper's `withWritableMint` (apps/server/src/keeper/scriptExtras.ts): the mint already among
/// the accounts is flipped writable (order unchanged); otherwise it is appended last, writable.
fn with_writable_mint(mut extra: Vec<AccountMeta>, mint: &Pubkey) -> Vec<AccountMeta> {
    let mut found = false;
    for a in extra.iter_mut().filter(|a| a.pubkey == *mint) {
        a.is_writable = true;
        found = true;
    }
    if !found {
        extra.push(AccountMeta::new(*mint, false));
    }
    extra
}

/// The token's hook slice for the creator's trades (empty without a hook).
fn slice(w: &World, mint: &Pubkey) -> Vec<AccountMeta> {
    let creator = companion::creator_address(mint);
    w.launch_base_slice(mint, &creator, &creator, true)
}

/// A burn step's accounts after the six: `aux`, the hook slice, the swap's accounts (a buy by the
/// creator, native or bridged), then the token program and its event authority (what the burn
/// needs that the swap's accounts may lack); the mint writable with `mint_writable`.
fn burn_accounts(w: &World, mint: &Pubkey, buys: bool, mint_writable: bool) -> Vec<AccountMeta> {
    let creator = companion::creator_address(mint);
    let k = keys(w, mint);
    let s = slice(w, mint);
    let mut extra = vec![AccountMeta::new_readonly(Pubkey::new_unique(), false)];
    extra.extend(s.clone());
    if buys {
        let ix = if w.launch(mint).is_native() {
            launch::swap_v2_with_base_slice(&k, creator, creator, 1, 0, 0, s)
        } else {
            launch::swap_with_base_slice(&k, creator, creator, 1, 0, 0, s)
        };
        extra.extend(metas(&ix));
    }
    extra.push(AccountMeta::new_readonly(token::event_authority(), false));
    extra.push(AccountMeta::new_readonly(bordrless_token::ID, false));
    if mint_writable {
        extra = with_writable_mint(extra, mint);
    }
    extra
}

/// A step in `mode` (buy `buy`, burn `burn`), the mint writable or not.
fn burn_step(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    mode: u8,
    buy: u64,
    burn: u64,
    mint_writable: bool,
) -> Tx {
    let n = slice(w, mint).len() as u8;
    let key = se::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    let bytes = se::burn_state_bytes(mode, buy, burn, n);
    a.data[..bytes.len()].copy_from_slice(&bytes);
    w.env.put(key, a);
    let extra = burn_accounts(w, mint, mode == se::MODE_BUY_BURN, mint_writable);
    let ix = companion::script_step(cranker.pubkey(), &keys(w, mint), SCRIPT, extra);
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// Buys with the pot and burns half of it: supply down by the burn, the creator keeps the rest,
/// the pot down by the buy and the bounty.
fn buys_and_burns(native: bool, taxed: bool) {
    let mut w = world(native);
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, taxed);
    assert_eq!(w.launch(&mint).is_native(), native);
    assert!(!keys(&w, &mint).burns, "no burn rate");
    let hook = w.env.read::<Mint>(&mint).hook_program;
    assert_eq!(hook == Some(tax_hook::ID), taxed);
    assert_eq!(slice(&w, &mint).is_empty(), hook.is_none());
    let creator = companion::creator_address(&mint);
    let cranker = fill_pot(&mut w, &mint);
    let pot = companion_of(&w, &mint).pending_pot;
    let supply_before = supply(&w, &mint);
    // Without the mint writable (the keeper without `mintWritable`) nothing moves.
    let tx = burn_step(&mut w, &cranker, &mint, se::MODE_BUY_BURN, BUY, 0, false);
    tx.err();
    let logs = tx.logs().join("\n");
    assert!(
        logs.contains("writable privilege escalated"),
        "mint read-only:\n{logs}"
    );
    assert_eq!(supply(&w, &mint), supply_before);
    assert_eq!(w.env.holding(&mint, &creator), 0);
    assert_eq!(companion_of(&w, &mint).pending_pot, pot);
    // A first step buys only, to know what a buy of BUY brings.
    let tx = burn_step(&mut w, &cranker, &mint, se::MODE_BUY_BURN, BUY, 0, true);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.tokens_before, 0);
    assert_eq!(ev.tokens_after, 0, "all it bought is burned");
    let burned_all = supply_before - supply(&w, &mint);
    assert!(burned_all > 0);
    assert_eq!(companion_of(&w, &mint).pending_pot, pot - BUY - SCRIPT_BOUNTY);
    // Then a buy that burns a part: the rest stays with the creator.
    w.env.warp(60);
    let s0 = supply(&w, &mint);
    let burn = burned_all / 3;
    let tx = burn_step(&mut w, &cranker, &mint, se::MODE_BUY_BURN, BUY, burn, true);
    tx.ok();
    let ev: ScriptStepped = tx.event();
    let bought = ev.tokens_after + burn;
    assert!(bought > burn);
    assert_eq!(w.env.holding(&mint, &creator), bought - burn);
    assert_eq!(s0 - supply(&w, &mint), burn);
}

#[test]
fn a_bridged_script_buys_and_burns_with_the_mint_writable() {
    buys_and_burns(false, false);
}

#[test]
fn a_native_script_buys_and_burns_with_the_mint_writable() {
    buys_and_burns(true, false);
}

#[test]
fn a_script_burns_a_custom_hook_coin_with_the_swap_slice() {
    buys_and_burns(false, true);
}

/// Burning into the dev bag is taking it.
#[test]
fn burning_the_dev_bag_is_refused() {
    let mut w = world(true);
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launched(&mut w, &launcher, false);
    let creator = companion::creator_address(&mint);
    let k = keys(&w, &mint);
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 4, 1)],
            &launcher,
            &[],
        )
        .ok();
    let bag = companion_of(&w, &mint).dev_tokens;
    assert!(bag > 0);
    let cranker = fill_pot(&mut w, &mint);
    let s0 = supply(&w, &mint);
    refused(
        &burn_step(&mut w, &cranker, &mint, se::MODE_BURN, 0, 1, true),
        CompanionError::ScriptTookDevBag,
    );
    refused(
        &burn_step(&mut w, &cranker, &mint, se::MODE_BURN, 0, 0, true),
        CompanionError::ScriptTookDevBag,
    );
    // A buy that burns one token more than it bought.
    let tx = burn_step(&mut w, &cranker, &mint, se::MODE_BUY_BURN, BUY, 0, true);
    tx.ok();
    let burned = s0 - supply(&w, &mint);
    w.env.warp(60);
    let s1 = supply(&w, &mint);
    let tx = burn_step(
        &mut w,
        &cranker,
        &mint,
        se::MODE_BUY_BURN,
        BUY,
        burned * 2,
        true,
    );
    refused(&tx, CompanionError::ScriptTookDevBag);
    assert_eq!(supply(&w, &mint), s1);
    assert_eq!(w.env.holding(&mint, &creator), bag);
}
