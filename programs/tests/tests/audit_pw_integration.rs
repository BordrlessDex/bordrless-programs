#![allow(clippy::type_complexity, clippy::needless_range_loop)]
//! Integration audit of payees and wallet payouts (2026-10-13), against the live companion.
//!
//! - **Differential:** a world is built with the new companion, then forked (LiteSVM is `Clone`)
//!   and the fork's companion replaced by the live binary (`PW_LIVE_SO`, default the scratchpad's
//!   `live-companion/out/bordrless_companion.so`). The same transaction is sent to both and their
//!   results compared: an existing flow must end in the same accounts.
//! - **Composition:** payees with buyback and holder splits, a game companion, a script companion
//!   (with wallet payouts on it), a refund before the launch, an unpayable payee.
//! - **Limits:** 10 payees and a 16-entry wallet answer, their CU, size (legacy and v0 with the
//!   protocol table) and CPI height; holder and wallet payouts in one window.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorSerialize, Discriminator};
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{
    CreateArgs, CreateGameArgs, CreateScriptArgs, PayeeArgs, PayoutTermsArgs,
};
use bordrless_companion::state::{
    Companion, GameKind, PayoutAnswer, PayoutEntry, PayoutTerms, Split, WalletPayoutAnswer,
    WalletPayoutEntry,
};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::{Env, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::orao;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_token::client as token;
use loss_cover_hook::client as lc_hook;
use lottery_hook::client as lottery;
use script_payouts as sp;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;
const FIX: Pubkey = sp::ID;
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
const PACKET: usize = 1232;

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

fn live_so() -> Vec<u8> {
    let path = std::env::var("PW_LIVE_SO").unwrap_or_else(|_| {
        "/tmp/claude-1000/-home-ubuntu-BORDRLESS/afd29fc2-f987-4995-8401-9e5be2ffe62e/scratchpad/live-companion/out/bordrless_companion.so".to_string()
    });
    std::fs::read(&path).unwrap_or_else(|e| panic!("read the live companion {path}: {e}"))
}

/// `w` forked, its companion replaced by `so` (upgradeable by the deployer, as `Env::new` leaves it).
fn fork_with(w: &World, so: &[u8]) -> World {
    let mut env = Env {
        svm: w.env.svm.clone(),
        payer: w.env.payer.insecure_clone(),
        now: w.env.now,
        slot: w.env.slot,
        deployer: w.env.deployer.insecure_clone(),
        treasury: w.env.treasury.insecure_clone(),
    };
    env.svm
        .add_program(bordrless_companion::ID, so)
        .expect("load the live companion");
    let deployer = env.deployer.pubkey();
    env.set_upgrade_authority(bordrless_companion::ID, Some(deployer));
    World { env, sol: w.sol }
}

/// The accounts `keys` as they stand: (lamports, data).
fn snapshot(w: &World, keys: &[Pubkey]) -> Vec<(Pubkey, Option<(u64, Vec<u8>)>)> {
    keys.iter()
        .map(|k| (*k, w.env.account(k).map(|a| (a.lamports, a.data))))
        .collect()
}

/// Sends `ixs` paid by `payer` (signing with `signers`) to both worlds; asserts the same outcome
/// and the same `watch` accounts after; answers (new, live).
fn both(
    new: &mut World,
    live: &mut World,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    watch: &[Pubkey],
    what: &str,
) -> (Tx, Tx) {
    let a = new.env.send_paid_by(ixs, payer, signers);
    let b = live.env.send_paid_by(ixs, payer, signers);
    assert_eq!(
        a.result.is_ok(),
        b.result.is_ok(),
        "{what}: outcome differs\nnew:\n{}\nlive:\n{}",
        a.logs().join("\n"),
        b.logs().join("\n")
    );
    assert_eq!(
        snapshot(new, watch),
        snapshot(live, watch),
        "{what}: accounts differ"
    );
    let events = |t: &Tx| {
        t.result.as_ref().ok().map(|_| {
            t.raw_events()
                .iter()
                .map(|e| (e.program, e.discriminator, e.body.clone()))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(events(&a), events(&b), "{what}: events differ");
    println!(
        "differential {what}: ok={} CU new {} live {} size {}",
        a.result.is_ok(),
        a.cu(),
        b.cu(),
        a.size
    );
    (a, b)
}

fn create_args(split: Split) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

/// A companion's launch with inline `rules` (no hook).
fn launch_plain(w: &mut World, launcher: &Keypair, mint: &Keypair, rules: LaunchRules) -> Tx {
    let creator = companion::creator_address(&mint.pubkey());
    let args = World::launch_args("PWAU", CREATOR_FEE, VQ, rules);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, args);
    w.env.send_paid_by(&[ix], launcher, &[mint])
}

/// A companion's launch from `config` (its custom hook's accounts from the registry).
fn launch_from(w: &mut World, launcher: &Keypair, mint: &Keypair, config: &Pubkey) -> Tx {
    let c = w.launch_config(config);
    let custom = c
        .custom_hook
        .map(|h| w.custom_hook_accounts(&h, &mint.pubkey()));
    let args = World::launch_args("PWAU", c.creator_fee_bps, VQ, c.rules);
    let inner = launch::create_launch_with(
        companion::creator_address(&mint.pubkey()),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        Some(*config),
        custom.as_ref(),
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, args);
    w.env.send_paid_by(&[ix], launcher, &[mint])
}

fn hook_config(w: &mut World, creator: &Keypair, hook: Pubkey, flags: u16) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(hook),
            custom_hook_flags: flags,
            label: "Audit".to_string(),
        },
    );
    tx.ok();
    config
}

fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

fn payee(wallet: Pubkey, bps: u16) -> PayeeArgs {
    PayeeArgs { wallet, bps }
}

fn answer_v(version: u8, entries: Vec<(u8, u64, u32)>) -> Vec<u8> {
    let mut out = Vec::new();
    PayoutAnswer {
        version,
        entries: entries
            .into_iter()
            .map(|(account, lamports, tag)| PayoutEntry {
                account,
                lamports,
                tag,
            })
            .collect(),
    }
    .serialize(&mut out)
    .unwrap();
    out
}

/// A wallet answer (version 2, 2026-10-13 revision: each entry names its account by key).
fn wallets(entries: Vec<(Pubkey, u64, u32)>) -> Vec<u8> {
    let mut out = Vec::new();
    WalletPayoutAnswer {
        version: PAYOUT_ANSWER_WALLETS_VERSION,
        entries: entries
            .into_iter()
            .map(|(to, lamports, tag)| WalletPayoutEntry { to, lamports, tag })
            .collect(),
    }
    .serialize(&mut out)
    .unwrap();
    out
}

fn set_state(w: &mut World, mint: &Pubkey, answer: &[u8]) {
    let key = sp::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    let bytes = sp::state_bytes(sp::MODE_ANSWER, 0, answer);
    a.data[..bytes.len()].copy_from_slice(&bytes);
    w.env.put(key, a);
}

fn script_world(w: &mut World) {
    w.env
        .svm
        .add_program(FIX, &program_bytes("script_payouts"))
        .expect("load the script");
    w.env.set_upgrade_authority(FIX, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &FIX);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                FIX,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
}

fn script_args() -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: FIX,
        interval: 60,
        // Room for a 16-entry wallet answer (709 bytes).
        state_len: 1_024,
        init_data: sp::state_bytes(sp::MODE_NOTHING, 0, &[]),
    }
}

const ALL_TO_BENEFICIARY: Split = Split {
    buyback_bps: 0,
    holders_bps: 0,
    beneficiary_bps: 10_000,
};

/// The protocol lookup table, written into `w`.
fn protocol_table(w: &mut World) -> solana_message::AddressLookupTableAccount {
    let addresses = protocol_lookup_table(w);
    w.env.put_lookup_table(Pubkey::new_unique(), &addresses)
}

// ---- 1. differential: existing flows, live vs new ------------------------------------------------

#[test]
fn differential_classic_withdraw_buyback_share_release_are_unchanged() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(20 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let split = Split {
        buyback_bps: 4_000,
        holders_bps: 3_000,
        beneficiary_bps: 3_000,
    };
    let mut args = create_args(split);
    args.vest_secs = 86_400;
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                args,
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    launch_plain(&mut w, &launcher, &mint, presets::burn()).ok();
    let k = keys(&w, &m);
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 2, 1)],
            &launcher,
            &[],
        )
        .ok();
    w.env.warp(61);
    trade(&mut w, &m, 6, 3 * SOL);
    let cranker = w.wallet_with_sol(5 * SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(3_600);
    let c = companion_of(&w, &m);
    assert!(c.pending_beneficiary > 0 && c.pending_buyback > 0 && c.pending_holders > 0);

    let mut live = fork_with(&w, &live_so());
    let creator = companion::creator_address(&m);
    let watch = [
        companion::companion_address(&m),
        creator,
        launcher.pubkey(),
        token::holding_address(&w.sol, &creator),
        token::holding_address(&m, &creator),
        token::holding_address(&m, &launcher.pubkey()),
    ];
    both(
        &mut w,
        &mut live,
        &[companion::withdraw(cranker.pubkey(), m, launcher.pubkey())],
        &cranker,
        &[],
        &watch,
        "withdraw v1",
    );
    // A second withdraw: nothing to do, on both.
    let (a, _) = both(
        &mut w,
        &mut live,
        &[companion::withdraw(cranker.pubkey(), m, launcher.pubkey())],
        &cranker,
        &[],
        &watch,
        "withdraw again",
    );
    a.expect_code(u32::from(CompanionError::NothingToDo));
    both(
        &mut w,
        &mut live,
        &[companion::share(cranker.pubkey(), m)],
        &cranker,
        &[],
        &watch,
        "share",
    );
    both(
        &mut w,
        &mut live,
        &[companion::buyback(cranker.pubkey(), &k, true)],
        &cranker,
        &[],
        &watch,
        "buyback",
    );
    both(
        &mut w,
        &mut live,
        &[companion::release(
            cranker.pubkey(),
            &k,
            true,
            launcher.pubkey(),
        )],
        &cranker,
        &[],
        &watch,
        "release",
    );
    // The refund path (pre-launch withdraw) of a v1 companion, on a fresh one.
    let mint2 = Keypair::new();
    let ixs = [companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint2.pubkey(),
        create_args(ALL_TO_BENEFICIARY),
    )];
    both(
        &mut w,
        &mut live,
        &ixs,
        &launcher,
        &[&mint2],
        &[companion::companion_address(&mint2.pubkey())],
        "create",
    );
    let mut refund = companion::withdraw(launcher.pubkey(), mint2.pubkey(), launcher.pubkey());
    refund
        .accounts
        .push(AccountMeta::new_readonly(mint2.pubkey(), true));
    let watch2 = [
        companion::companion_address(&mint2.pubkey()),
        companion::creator_address(&mint2.pubkey()),
        launcher.pubkey(),
    ];
    both(
        &mut w,
        &mut live,
        &[refund],
        &launcher,
        &[&mint2],
        &watch2,
        "refund v1",
    );
    // set_payees does not exist on the live program.
    let mint3 = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint3.pubkey(),
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(
            launcher.pubkey(),
            mint3.pubkey(),
            vec![payee(Pubkey::new_unique(), 1)],
        ),
    ];
    assert!(w
        .env
        .send_paid_by(&ixs, &launcher, &[&mint3])
        .result
        .is_ok());
    assert!(live
        .env
        .send_paid_by(&ixs, &launcher, &[&mint3])
        .result
        .is_err());
}

/// A Loss cover-hook coin running the payout fixture with `terms`; its pot filled and two holders.
struct HookCoin {
    w: World,
    mint: Pubkey,
    cranker: Keypair,
    a: Keypair,
    b: Keypair,
}

fn hook_coin(terms: PayoutTermsArgs, payees: &[PayeeArgs], allow: bool) -> HookCoin {
    let mut w = World::new();
    script_world(&mut w);
    let launcher = w.wallet_with_sol(10 * SOL);
    let config = hook_config(
        &mut w,
        &launcher,
        loss_cover_hook::ID,
        loss_cover_hook::FLAGS,
    );
    let mint = Keypair::new();
    let m = mint.pubkey();
    let mut ixs = vec![
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        lc_hook::prepare(launcher.pubkey(), m),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), terms, vec![]),
    ];
    if !payees.is_empty() {
        ixs.push(companion::set_payees(launcher.pubkey(), m, payees.to_vec()));
    }
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    if allow {
        w.env
            .send_paid_by(&[companion::allow_wallet_payouts(m)], &launcher, &[&mint])
            .ok();
    }
    launch_from(&mut w, &launcher, &mint, &config).ok();
    w.env.warp(31);
    trade(&mut w, &m, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    let a = w.wallet_with_sol(2 * SOL);
    w.buy(&a, &m, SOL).ok();
    let b = w.wallet_with_sol(2 * SOL);
    w.buy(&b, &m, SOL / 2).ok();
    w.env.warp(61);
    HookCoin {
        w,
        mint: m,
        cranker,
        a,
        b,
    }
}

const FIRST: u8 = 7;

/// A step of the fixture on a hook coin: the payout accounts (7), then `extra`.
fn hook_step(w: &World, cranker: Pubkey, mint: &Pubkey, extra: Vec<AccountMeta>) -> Instruction {
    let mut accounts = companion::payout_accounts(mint, &loss_cover_hook::ID);
    assert_eq!(accounts.len(), usize::from(FIRST));
    accounts.extend(extra);
    companion::script_step(cranker, &keys(w, mint), FIX, accounts)
}

#[test]
fn differential_holder_payouts_are_unchanged_and_wallet_answers_stay_off() {
    let mut c = hook_coin(
        PayoutTermsArgs {
            step_bps: 5_000,
            holding_bps: 5_000,
            window_bps: 10_000,
            window_secs: 300,
        },
        &[],
        false,
    );
    let m = c.mint;
    let pot = companion_of(&c.w, &m).pending_pot;
    let x = pot / 20;
    // Version 1: pay a (holding + owner), touch b (holding alone).
    let extra = vec![
        AccountMeta::new(token::holding_address(&m, &c.a.pubkey()), false),
        AccountMeta::new(c.a.pubkey(), false),
        AccountMeta::new(token::holding_address(&m, &c.b.pubkey()), false),
    ];
    set_state(
        &mut c.w,
        &m,
        &answer_v(1, vec![(FIRST, x, 1), (FIRST + 2, 0, 2)]),
    );
    let mut live = fork_with(&c.w, &live_so());
    let ix = hook_step(&c.w, c.cranker.pubkey(), &m, extra.clone());
    let watch = [
        companion::companion_address(&m),
        companion::payout_terms_address(&m),
        companion::creator_address(&m),
        c.a.pubkey(),
        c.cranker.pubkey(),
        token::holding_address(&m, &c.a.pubkey()),
        token::holding_address(&m, &c.b.pubkey()),
        sp::state_address(&m),
    ];
    let cranker = c.cranker.insecure_clone();
    let (a, _) = both(
        &mut c.w,
        &mut live,
        &[ix],
        &cranker,
        &[],
        &watch,
        "holder payout step v1",
    );
    a.ok();
    // A wallet answer on a coin whose launcher never allowed them (every coin made before): nothing
    // moves on either; live refuses the version (BadPayoutAnswer), new refuses the payouts
    // (WalletPayoutsOff).
    c.w.env.warp(61);
    live.env.warp(61);
    let v2 = wallets(vec![(c.a.pubkey(), x / 2, 9)]);
    set_state(&mut c.w, &m, &v2);
    set_state(&mut live, &m, &v2);
    let ix = hook_step(&c.w, cranker.pubkey(), &m, extra.clone());
    let (n, l) = both(
        &mut c.w,
        &mut live,
        &[ix],
        &cranker,
        &[],
        &watch,
        "wallet answer, not allowed",
    );
    refused(&l, CompanionError::BadPayoutAnswer);
    refused(&n, CompanionError::WalletPayoutsOff);
    // The first revision's index layout (13-byte entries) as version 2: malformed on both.
    let old = answer_v(2, vec![(FIRST + 1, x / 2, 9)]);
    set_state(&mut c.w, &m, &old);
    set_state(&mut live, &m, &old);
    let ix = hook_step(&c.w, cranker.pubkey(), &m, extra);
    let (n, _) = both(
        &mut c.w,
        &mut live,
        &[ix],
        &cranker,
        &[],
        &watch,
        "index-layout version 2",
    );
    refused(&n, CompanionError::BadPayoutAnswer);
    // A launched coin can't be allowed wallet payouts after the fact (its launcher still holds the mint? no:
    // the mint's keypair is gone once launched in practice; the program refuses it regardless).
    println!("existing script companions can't start paying wallets: WalletPayoutsOff");
}

// ---- 2. payees composed with every kind of companion ---------------------------------------------

#[test]
fn payees_with_buyback_and_holder_splits_share_only_the_beneficiary_part() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(20 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let (p1, p2) = (w.wallet_with_sol(SOL), w.wallet_with_sol(SOL));
    let split = Split {
        buyback_bps: 4_000,
        holders_bps: 3_000,
        beneficiary_bps: 3_000,
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args(split)),
        companion::set_payees(
            launcher.pubkey(),
            m,
            vec![payee(p1.pubkey(), 5_000), payee(p2.pubkey(), 2_500)],
        ),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_plain(&mut w, &launcher, &mint, presets::burn()).ok();
    w.env.warp(61);
    trade(&mut w, &m, 6, 3 * SOL);
    let cranker = w.wallet_with_sol(5 * SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), m, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let claimed: FeesClaimed = tx.event();
    let c = companion_of(&w, &m);
    assert_eq!(c.pending_beneficiary, claimed.to_beneficiary);
    // Buyback and share are untouched by the payees.
    let k = keys(&w, &m);
    w.env
        .send_paid_by(
            &[companion::buyback(cranker.pubkey(), &k, true)],
            &cranker,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(&[companion::share(cranker.pubkey(), m)], &cranker, &[])
        .ok();
    let c2 = companion_of(&w, &m);
    assert_eq!(c2.pending_beneficiary, c.pending_beneficiary);
    assert_eq!(c2.pending_holders, 0);
    // Withdraw: the payees take their bps of the beneficiary part only.
    let before = [p1.pubkey(), p2.pubkey()].map(|k| w.env.lamports(&k));
    let ix = companion::withdraw_with_payees(
        cranker.pubkey(),
        m,
        launcher.pubkey(),
        &[p1.pubkey(), p2.pubkey()],
    );
    w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    let part = c.pending_beneficiary;
    assert_eq!(w.env.lamports(&p1.pubkey()) - before[0], part / 2);
    assert_eq!(w.env.lamports(&p2.pubkey()) - before[1], part / 4);
    assert_eq!(companion_of(&w, &m).pending_beneficiary, 0);
}

#[test]
fn a_game_companion_with_payees() {
    let mut w = World::new();
    orao::load(&mut w.env);
    let launcher = w.wallet_with_sol(50 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let p1 = w.wallet_with_sol(SOL);
    let g = CreateGameArgs {
        kind: GameKind::Lottery,
        hook: lottery_hook::ID,
        split: Split {
            buyback_bps: 2_000,
            holders_bps: 0,
            beneficiary_bps: 1_000,
        },
        pot_bps: 7_000,
        round_secs: 3_600,
        min_pot: 100_000_000,
        prize_bps: 10_000,
        claim_window_secs: 300,
        max_attempts: 6,
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(Split {
                buyback_bps: 10_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            }),
        ),
        lottery::prepare(launcher.pubkey(), m, g.round_secs),
        companion::create_game(launcher.pubkey(), m, g),
        companion::set_payees(launcher.pubkey(), m, vec![payee(p1.pubkey(), 4_000)]),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    println!(
        "game setup with payees (create, prepare, create_game, set_payees): CU {} size {}",
        tx.cu(),
        tx.size
    );
    let config = hook_config(&mut w, &launcher, lottery_hook::ID, lottery_hook::FLAGS);
    launch_from(&mut w, &launcher, &mint, &config).ok();
    w.env.warp(31);
    for _ in 0..2 {
        let t = w.wallet_with_sol(6 * SOL);
        w.buy(&t, &m, 5 * SOL).ok();
        let held = w.env.holding(&m, &t.pubkey());
        w.sell(&t, &m, held).ok();
    }
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_game(
                cranker.pubkey(),
                m,
                lottery_hook::ID,
            )],
            &cranker,
            &[],
        )
        .ok();
    let c = companion_of(&w, &m);
    assert!(c.pending_beneficiary > 0 && c.pending_pot > 0);
    assert_eq!(c.version, VERSION_WITH_PAYEES);
    let ix = companion::withdraw(cranker.pubkey(), m, launcher.pubkey());
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayeesMissing,
    );
    let before = w.env.lamports(&p1.pubkey());
    let ix =
        companion::withdraw_with_payees(cranker.pubkey(), m, launcher.pubkey(), &[p1.pubkey()]);
    w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    assert_eq!(
        w.env.lamports(&p1.pubkey()) - before,
        c.pending_beneficiary * 4_000 / 10_000
    );
    // The pot is untouched.
    assert_eq!(companion_of(&w, &m).pending_pot, c.pending_pot);
}

#[test]
fn a_script_companion_with_payees_and_wallet_payouts() {
    let mut w = World::new();
    script_world(&mut w);
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let p1 = w.wallet_with_sol(SOL);
    let terms = PayoutTermsArgs {
        step_bps: 5_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: 300,
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), terms, vec![]),
        companion::set_payees(launcher.pubkey(), m, vec![payee(p1.pubkey(), 10_000)]),
        companion::allow_wallet_payouts(m),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    println!(
        "script setup with payees and wallet payouts allowed: CU {} size {}",
        tx.cu(),
        tx.size
    );
    launch_plain(&mut w, &launcher, &mint, LaunchRules::NONE).ok();
    w.env.warp(31);
    trade(&mut w, &m, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    let c = companion_of(&w, &m);
    assert!(c.pending_beneficiary > 0 && c.pending_pot > 0);
    // All of the beneficiary part to the one payee: the beneficiary gets nothing but the spare SOL.
    let before = w.env.lamports(&p1.pubkey());
    let ix =
        companion::withdraw_with_payees(cranker.pubkey(), m, launcher.pubkey(), &[p1.pubkey()]);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    assert_eq!(w.env.lamports(&p1.pubkey()) - before, c.pending_beneficiary);
    let paid: BeneficiaryPaid = tx.event();
    println!(
        "withdraw, one payee taking it all: beneficiary paid {} (spare SOL)",
        paid.lamports
    );
    // The pot pays a wallet in the same coin.
    w.env.warp(61);
    let alice = w.env.funded(SOL);
    set_state(
        &mut w,
        &m,
        &wallets(vec![(alice.pubkey(), c.pending_pot / 10, 0)]),
    );
    let accounts = vec![
        AccountMeta::new(companion::payout_terms_address(&m), false),
        AccountMeta::new(alice.pubkey(), false),
    ];
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    assert_eq!(w.env.lamports(&alice.pubkey()), SOL + c.pending_pot / 10);
    // The payees account (the companion's own) is no wallet-payout target.
    w.env.warp(61);
    let payees_pda = companion::payees_address(&m);
    set_state(&mut w, &m, &wallets(vec![(payees_pda, 1_000_000, 0)]));
    let accounts = vec![
        AccountMeta::new(companion::payout_terms_address(&m), false),
        AccountMeta::new(payees_pda, false),
    ];
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::WalletPayoutTarget,
    );
}

#[test]
fn a_refund_before_the_launch_works_without_the_payees() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let p1 = Pubkey::new_unique();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher.pubkey(), m, vec![payee(p1, 5_000)]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    // The SDK's `companion.refund` today: withdraw + the mint signing, no payees: it works (nothing
    // pending, so no split).
    let mut refund = companion::withdraw(launcher.pubkey(), m, launcher.pubkey());
    refund.accounts.push(AccountMeta::new_readonly(m, true));
    let before = w.env.lamports(&launcher.pubkey());
    w.env.send_paid_by(&[refund], &launcher, &[&mint]).ok();
    assert!(w.env.lamports(&launcher.pubkey()) > before);
}

#[test]
fn an_unpayable_payee_is_owed_and_an_empty_withdraw_is_refused() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    // A program id (executable) can be set as a payee: nothing checks it at set_payees.
    let program_payee = bordrless_token::ID;
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher.pubkey(), m, vec![payee(program_payee, 5_000)]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_plain(&mut w, &launcher, &mint, LaunchRules::NONE).ok();
    w.env.warp(31);
    trade(&mut w, &m, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    let pending = companion_of(&w, &m).pending_beneficiary;
    let ix = || {
        companion::withdraw_with_payees(cranker.pubkey(), m, launcher.pubkey(), &[program_payee])
    };
    let tx = w.env.send_paid_by(&[ix()], &cranker, &[]);
    tx.ok();
    let owed = companion_of(&w, &m).pending_beneficiary;
    assert_eq!(owed, pending / 2, "half owed to the program id");
    // Fixed after this audit: a withdraw that would pay nobody is refused (NothingToDo), so the
    // keeper's simulation fails and it never pays fees to resend it.
    let tx = w.env.send_paid_by(&[ix()], &cranker, &[]);
    tx.expect_code(u32::from(CompanionError::NothingToDo));
    assert_eq!(companion_of(&w, &m).pending_beneficiary, owed);
}

// ---- 3. limits ----------------------------------------------------------------------------------

#[test]
fn ten_payees_withdraw_fits_one_transaction() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let wallets: Vec<Keypair> = (0..10).map(|_| w.env.funded(SOL)).collect();
    let list: Vec<PayeeArgs> = wallets.iter().map(|k| payee(k.pubkey(), 1_000)).collect();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher.pubkey(), m, list),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    println!("create + set_payees (10): CU {} size {}", tx.cu(), tx.size);
    launch_plain(&mut w, &launcher, &mint, LaunchRules::NONE).ok();
    w.env.warp(31);
    trade(&mut w, &m, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    let pending = companion_of(&w, &m).pending_beneficiary;
    let keys_: Vec<Pubkey> = wallets.iter().map(|k| k.pubkey()).collect();
    let ix = companion::withdraw_with_payees(cranker.pubkey(), m, launcher.pubkey(), &keys_);
    let table = protocol_table(&mut w);
    let v0 = w
        .env
        .v0_size(std::slice::from_ref(&ix), &cranker, &[], &[table]);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    for k in &wallets {
        assert_eq!(w.env.lamports(&k.pubkey()), SOL + pending / 10);
    }
    assert_eq!(companion_of(&w, &m).pending_beneficiary, 0);
    // PayeePaid is a self-CPI (as every other companion event), so the indexer sees it.
    let cpi = tx
        .raw_events()
        .iter()
        .filter(|e| e.discriminator == PayeePaid::DISCRIMINATOR)
        .count();
    let logged = tx
        .logs()
        .iter()
        .filter(|l| l.starts_with("Program data: "))
        .count();
    println!(
        "withdraw, 10 payees: CU {} legacy size {} (with the 1.4M CU limit ix) v0 with the protocol table {} height {} trace {}; PayeePaid as CPI events {}, 'Program data' log lines {}",
        tx.cu(),
        tx.size,
        v0,
        tx.max_height(),
        tx.trace_len(),
        cpi,
        logged
    );
    assert!(tx.cu() < 1_400_000);
    assert!(tx.size <= PACKET, "10 payees, legacy: {} bytes", tx.size);
    assert_eq!(cpi, 10);
    let _ = logged;
}

/// A plain coin's script companion with terms (all caps 100%) and its pot filled.
fn wallet_coin(w: &mut World) -> (Pubkey, Keypair) {
    script_world(w);
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let terms = PayoutTermsArgs {
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: 300,
    };
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), terms, vec![]),
        companion::allow_wallet_payouts(m),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_plain(w, &launcher, &mint, LaunchRules::NONE).ok();
    w.env.warp(31);
    trade(w, &m, 6, 3 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(61);
    (m, cranker)
}

#[test]
fn sixteen_wallet_payouts_in_one_step() {
    let mut w = World::new();
    let (m, cranker) = wallet_coin(&mut w);
    let pot = companion_of(&w, &m).pending_pot;
    let ws: Vec<Keypair> = (0..16).map(|_| w.env.funded(SOL)).collect();
    let each = pot / 40;
    let entries: Vec<(Pubkey, u64, u32)> = ws
        .iter()
        .enumerate()
        .map(|(i, k)| (k.pubkey(), each, i as u32))
        .collect();
    set_state(&mut w, &m, &wallets(entries));
    let wallets = ws;
    let mut accounts = vec![AccountMeta::new(companion::payout_terms_address(&m), false)];
    accounts.extend(wallets.iter().map(|k| AccountMeta::new(k.pubkey(), false)));
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    let table = protocol_table(&mut w);
    let v0 = w.env.v0_size(
        std::slice::from_ref(&ix),
        &cranker,
        &[],
        std::slice::from_ref(&table),
    );
    // A per-step table holding the 16 wallets as well (what a keeper would need to fit them).
    let mut all = protocol_lookup_table(&w);
    all.extend(wallets.iter().map(|k| k.pubkey()));
    all.push(companion::payout_terms_address(&m));
    let big = w.env.put_lookup_table(Pubkey::new_unique(), &all);
    let v0_wallets = w
        .env
        .v0_size(std::slice::from_ref(&ix), &cranker, &[], &[big]);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    for k in &wallets {
        assert_eq!(w.env.lamports(&k.pubkey()), SOL + each);
    }
    assert_eq!(tx.events::<ScriptWalletPayout>().len(), 16);
    println!(
        "wallet payout step, 16 entries: CU {} legacy size {} v0 with the protocol table {} v0 with the wallets in a table {} height {} trace {}",
        tx.cu(),
        tx.size,
        v0,
        v0_wallets,
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.cu() < 1_400_000);
    assert!(tx.max_height() <= 5);
}

#[test]
fn holder_and_wallet_payouts_share_one_window() {
    let mut c = hook_coin(
        PayoutTermsArgs {
            step_bps: 10_000,
            holding_bps: 10_000,
            window_bps: 5_000,
            window_secs: 3_600,
        },
        &[],
        true,
    );
    let m = c.mint;
    // Start at the beginning of a window, so both steps fall in it.
    let into = c.w.env.now.rem_euclid(3_600);
    c.w.env.warp(3_600 - into + 1);
    let pot = companion_of(&c.w, &m).pending_pot;
    let x = pot / 5;
    let extra = vec![
        AccountMeta::new(token::holding_address(&m, &c.a.pubkey()), false),
        AccountMeta::new(c.a.pubkey(), false),
    ];
    set_state(&mut c.w, &m, &answer_v(1, vec![(FIRST, x, 1)]));
    let ix = hook_step(&c.w, c.cranker.pubkey(), &m, extra);
    let cranker = c.cranker.insecure_clone();
    c.w.env.send_paid_by(&[ix], &cranker, &[]).ok();
    let t: PayoutTerms = c.w.env.read(&companion::payout_terms_address(&m));
    assert_eq!(t.window_paid, x);
    let left = t.window_pot / 2 - x;
    // A wallet payout one lamport over what the window has left: refused; exactly it: paid.
    c.w.env.warp(61);
    let bob = c.w.wallet_with_sol(SOL);
    let wallet = vec![AccountMeta::new(bob.pubkey(), false)];
    set_state(&mut c.w, &m, &wallets(vec![(bob.pubkey(), left + 1, 2)]));
    let ix = hook_step(&c.w, cranker.pubkey(), &m, wallet.clone());
    refused(
        &c.w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayoutOverBound,
    );
    set_state(&mut c.w, &m, &wallets(vec![(bob.pubkey(), left, 2)]));
    let ix = hook_step(&c.w, cranker.pubkey(), &m, wallet);
    let tx = c.w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    let t: PayoutTerms = c.w.env.read(&companion::payout_terms_address(&m));
    assert_eq!(
        (t.window_paid, t.payouts, t.paid_total),
        (x + left, 2, x + left)
    );
    // The window is spent: a holder payout of one lamport is refused too.
    c.w.env.warp(61);
    let b_hold = vec![
        AccountMeta::new(token::holding_address(&m, &c.b.pubkey()), false),
        AccountMeta::new(c.b.pubkey(), false),
    ];
    set_state(&mut c.w, &m, &answer_v(1, vec![(FIRST, 1, 3)]));
    let ix = hook_step(&c.w, cranker.pubkey(), &m, b_hold);
    refused(
        &c.w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayoutOverBound,
    );
    // A wallet payout on a hook coin can name a holder's owner: no record is written for it.
    println!(
        "holder + wallet payouts share window_paid/payouts/paid_total; wallet step CU {}",
        tx.cu()
    );
}

/// Wire sizes as the keeper compiles them (v0, the protocol table, a unit limit and a priority
/// fee): a wallet step by its number of wallets, and a withdraw by its number of payees.
#[test]
fn sizes_by_count_as_the_keeper_sends_them() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    let mut w = World::new();
    let (m, cranker) = wallet_coin(&mut w);
    let table = protocol_table(&mut w);
    let budget = [compute_unit_limit(400_000), compute_unit_price(1_000)];
    let mut fits = (0, 0);
    for n in 1..=16usize {
        let mut accounts = vec![AccountMeta::new(companion::payout_terms_address(&m), false)];
        accounts.extend((0..n).map(|_| AccountMeta::new(Pubkey::new_unique(), false)));
        let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
        let mut ixs = budget.to_vec();
        ixs.push(ix);
        let bare = w.env.v0_size(&ixs, &cranker, &[], &[]);
        let with = w
            .env
            .v0_size(&ixs, &cranker, &[], std::slice::from_ref(&table));
        if bare <= PACKET {
            fits.0 = n;
        }
        if with <= PACKET {
            fits.1 = n;
        }
        println!("wallet step, {n} wallets: {bare} bytes without a table, {with} with the protocol table");
    }
    println!(
        "wallet step: at most {} wallets without a table, {} with the protocol table",
        fits.0, fits.1
    );
    let mut fits = (0, 0);
    for n in 1..=10usize {
        let payees: Vec<Pubkey> = (0..n).map(|_| Pubkey::new_unique()).collect();
        let ix =
            companion::withdraw_with_payees(cranker.pubkey(), m, Pubkey::new_unique(), &payees);
        let mut ixs = budget.to_vec();
        ixs.push(ix);
        let bare = w.env.v0_size(&ixs, &cranker, &[], &[]);
        let with = w
            .env
            .v0_size(&ixs, &cranker, &[], std::slice::from_ref(&table));
        if bare <= PACKET {
            fits.0 = n;
        }
        if with <= PACKET {
            fits.1 = n;
        }
        if n == 10 {
            println!(
                "withdraw, 10 payees: {bare} bytes without a table, {with} with the protocol table"
            );
        }
    }
    println!(
        "withdraw: at most {} payees without a table, {} with the protocol table",
        fits.0, fits.1
    );
}

/// Where `set_payees` fits: in a setup transaction beside `create` (and a game's or a script's
/// setup), by its number of payees, with a unit limit, without and with the protocol table.
#[test]
fn set_payees_in_the_setup_transaction() {
    use bordrless_program_tests::env::compute_unit_limit;
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let table = protocol_table(&mut w);
    let terms = PayoutTermsArgs {
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: 300,
    };
    let mut lc_args = script_args();
    lc_args.program = loss_cover_script::ID;
    lc_args.init_data = vec![0u8; 64];
    let setups: [(&str, Vec<Instruction>); 3] = [
        (
            "classic",
            vec![companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(ALL_TO_BENEFICIARY),
            )],
        ),
        (
            "loss cover (create, prepare, create_script_v2)",
            vec![
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    m,
                    create_args(ALL_TO_BENEFICIARY),
                ),
                lc_hook::prepare(launcher.pubkey(), m),
                companion::create_script_v2(launcher.pubkey(), m, lc_args, terms, vec![]),
            ],
        ),
        (
            "lottery (create, prepare, create_game)",
            vec![
                companion::create(
                    launcher.pubkey(),
                    launcher.pubkey(),
                    m,
                    create_args(ALL_TO_BENEFICIARY),
                ),
                lottery::prepare(launcher.pubkey(), m, 3_600),
                companion::create_game(
                    launcher.pubkey(),
                    m,
                    CreateGameArgs {
                        kind: GameKind::Lottery,
                        hook: lottery_hook::ID,
                        split: Split {
                            buyback_bps: 2_000,
                            holders_bps: 0,
                            beneficiary_bps: 1_000,
                        },
                        pot_bps: 7_000,
                        round_secs: 3_600,
                        min_pot: 100_000_000,
                        prize_bps: 10_000,
                        claim_window_secs: 300,
                        max_attempts: 6,
                    },
                ),
            ],
        ),
    ];
    for (name, setup) in setups {
        let mut most = (0, 0);
        for n in 0..=10usize {
            let mut ixs = vec![compute_unit_limit(400_000)];
            ixs.extend(setup.iter().cloned());
            if n > 0 {
                let list: Vec<PayeeArgs> =
                    (0..n).map(|_| payee(Pubkey::new_unique(), 100)).collect();
                ixs.push(companion::set_payees(launcher.pubkey(), m, list));
            }
            let bare = w.env.v0_size(&ixs, &launcher, &[&mint], &[]);
            let with = w
                .env
                .v0_size(&ixs, &launcher, &[&mint], std::slice::from_ref(&table));
            if bare <= PACKET {
                most.0 = n;
            }
            if with <= PACKET {
                most.1 = n;
            }
            if n == 0 || n == 1 || n == 10 {
                println!("{name} + set_payees({n}): {bare} bytes, {with} with the protocol table");
            }
        }
        println!(
            "{name}: at most {} payees in the setup without a table, {} with it",
            most.0, most.1
        );
    }
}

/// What the fixture's buy needs among the step's accounts (as `companion_payouts.rs`).
fn buy_accounts(w: &World, mint: &Pubkey) -> Vec<AccountMeta> {
    let creator = companion::creator_address(mint);
    let ix = w.launch_swap_to_ix(&creator, &creator, mint, 1, 0);
    let mut metas: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    metas.push(AccountMeta::new_readonly(ix.program_id, false));
    metas.push(AccountMeta::new_readonly(
        bordrless_hook::hook_accounts_address(&loss_cover_hook::ID, mint).0,
        false,
    ));
    metas
}

/// The heaviest wallet step: a hook coin's script buys, then answers 16 wallets (the transaction
/// is over the packet without a table naming the wallets; LiteSVM sends it anyway): the heap, the
/// units and the height hold.
#[test]
fn a_buy_then_sixteen_wallet_payouts_on_a_hook_coin() {
    let mut c = hook_coin(
        PayoutTermsArgs {
            step_bps: 10_000,
            holding_bps: 10_000,
            window_bps: 10_000,
            window_secs: 300,
        },
        &[],
        true,
    );
    let m = c.mint;
    let pot = companion_of(&c.w, &m).pending_pot;
    let ws: Vec<Keypair> = (0..16).map(|_| c.w.env.funded(SOL)).collect();
    let mut extra = buy_accounts(&c.w, &m);
    extra.extend(ws.iter().map(|k| AccountMeta::new(k.pubkey(), false)));
    let buy = pot / 10;
    let each = pot / 50;
    let entries: Vec<(Pubkey, u64, u32)> = ws
        .iter()
        .enumerate()
        .map(|(i, k)| (k.pubkey(), each, i as u32))
        .collect();
    let key = sp::state_address(&m);
    let mut a = c.w.env.account(&key).unwrap();
    let bytes = sp::state_bytes(sp::MODE_BUY_THEN_ANSWER, buy, &wallets(entries));
    let wallets = ws;
    a.data[..bytes.len()].copy_from_slice(&bytes);
    c.w.env.put(key, a);
    let ix = hook_step(&c.w, c.cranker.pubkey(), &m, extra);
    let cranker = c.cranker.insecure_clone();
    let tx = c.w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    for k in &wallets {
        assert_eq!(c.w.env.lamports(&k.pubkey()), SOL + each);
    }
    println!(
        "a buy then 16 wallet payouts on a hook coin: CU {} size {} height {} trace {}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.cu() < 1_400_000);
    assert!(tx.max_height() <= 5);
}
