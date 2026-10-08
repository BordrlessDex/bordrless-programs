//! Studio's hook template on a real launch (the monorepo's apps/server/src/studio/starters, built
//! by the Studio build worker in the verifiable-build image for the fixed test ids below; the
//! binaries are in programs/tests/fixtures). Every Studio hook has the same `prepare`, which a
//! launch sends for its new mint before creating it, exactly as for Half-Life:
//!
//! - `blank` takes nothing: a launch from a config naming it trades as a plain one.
//! - `sell-tax` takes 3% of every sell (a transfer into the launch pool) to the wallet that prepared
//!   it, once its holding exists; buys, wallet-to-wallet transfers and the launch's own moves pass
//!   free.
//!
//! Rebuild a fixture with the worker (tools/studio-builder) for the same program id when a starter
//! changes.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_hook::{hook_accounts_address, token_flags};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::SYSTEM_PROGRAM_ID;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_swap::events::Swapped;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const BLANK: Pubkey = Pubkey::from_str_const("CUeDz3reW3KTZk27Fq1j5G9AP3gD5JtbqV3G7jMkwtVk");
const SELL_TAX: Pubkey = Pubkey::from_str_const("AZW4fVebvGX45YCcYBHmRcR26P6w4CHtMWFGVZGe7wL");
/// `sha256("global:prepare")[..8]`.
const PREPARE: [u8; 8] = [0x79, 0x9b, 0x9c, 0x5a, 0xa4, 0xfc, 0xdc, 0x6d];

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/fixtures/{name}.so", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The standard `prepare` of a Studio hook: `[payer, mint, state = ["state", mint], registry, system]`.
fn prepare_ix(hook: Pubkey, payer: Pubkey, mint: Pubkey) -> Instruction {
    let state = Pubkey::find_program_address(&[b"state", mint.as_ref()], &hook).0;
    Instruction {
        program_id: hook,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(state, false),
            AccountMeta::new(hook_accounts_address(&hook, &mint).0, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: PREPARE.to_vec(),
    }
}

/// A world with both Studio hooks loaded.
fn world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(BLANK, &fixture("studio_blank"))
        .expect("load blank");
    w.env
        .svm
        .add_program(SELL_TAX, &fixture("studio_sell_tax"))
        .expect("load sell-tax");
    w
}

/// A launch by a fresh creator whose token runs `hook` with `flags`: prepared by the creator,
/// launched from a config naming it, past the sniper window. Answers the creator and the mint.
fn launch_with(w: &mut World, hook: Pubkey, flags: u16, symbol: &str) -> (Keypair, Pubkey) {
    let creator = w.wallet_with_sol(30 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(&[prepare_ix(hook, creator.pubkey(), mint)], &creator, &[])
        .ok();
    let (config, tx) = w.create_config(
        &creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(hook),
            custom_hook_flags: flags,
            label: symbol.to_string(),
        },
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, symbol, VQ, &config);
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert_eq!(w.launch(&mint).custom_hook, Some(hook));
    w.env.warp(31);
    (creator, mint)
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

#[test]
fn prepare_is_the_standard_one_and_once_per_mint() {
    let mut w = world();
    let payer = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new().pubkey();
    w.env
        .send_paid_by(&[prepare_ix(SELL_TAX, payer.pubkey(), mint)], &payer, &[])
        .ok();
    let registry = w
        .env
        .account(&hook_accounts_address(&SELL_TAX, &mint).0)
        .expect("the registry");
    assert_eq!(registry.owner, SELL_TAX);
    let state = w
        .env
        .account(&Pubkey::find_program_address(&[b"state", mint.as_ref()], &SELL_TAX).0)
        .expect("the state");
    assert_eq!(state.owner, SELL_TAX);
    // Once per mint: what was prepared stays.
    w.env.warp(1);
    w.env
        .send_paid_by(&[prepare_ix(SELL_TAX, payer.pubkey(), mint)], &payer, &[])
        .expect_fail();
}

#[test]
fn a_blank_hook_launches_and_trades_as_a_plain_token() {
    let mut w = world();
    let (_, mint) = launch_with(&mut w, BLANK, token_flags::BEFORE_TRANSFER, "BLNK");
    let t = trader(&mut w, &mint, 10 * SOL);
    let tx = w.buy(&t, &mint, SOL);
    tx.ok();
    // A buy's input cut is the launch's 1% creator fee, in SOL; the hook takes nothing from the tokens out.
    let ev: Swapped = tx.event();
    assert_eq!((ev.cuts_in, ev.cuts_out), (SOL / 100, 0));
    let held = w.env.holding(&mint, &t.pubkey());
    assert!(held > 0);
    let tx = w.sell(&t, &mint, held / 2);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!((ev.cuts_in, ev.received_in), (0, held / 2));
}

#[test]
fn a_sell_tax_hook_takes_three_percent_of_sells_to_its_collector() {
    let mut w = world();
    let flags = token_flags::BEFORE_TRANSFER | token_flags::TRANSFER_RETURNS_DELTA;
    let (creator, mint) = launch_with(&mut w, SELL_TAX, flags, "TAX");
    let t = trader(&mut w, &mint, 10 * SOL);
    w.buy(&t, &mint, SOL).ok();
    let held = w.env.holding(&mint, &t.pubkey());

    // Until the collector (the creator, who prepared it) holds the token, sells pass free.
    let tx = w.sell(&t, &mint, held / 10);
    tx.ok();
    assert_eq!(tx.event::<Swapped>().cuts_in, 0);

    // The creator buys: their holding now exists, and buys are never taxed.
    w.holdings(&creator, mint, &[creator.pubkey()]);
    let tx = w.buy(&creator, &mint, SOL / 10);
    tx.ok();
    assert_eq!(tx.event::<Swapped>().cuts_out, 0);
    let collector_before = w.env.holding(&mint, &creator.pubkey());

    // A sell pays 3% of the tokens sold to the collector; the pool receives the rest.
    let amount = held / 4;
    let tax = amount * 300 / 10_000;
    let tx = w.sell(&t, &mint, amount);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!((ev.cuts_in, ev.received_in), (tax, amount - tax));
    assert_eq!(
        w.env.holding(&mint, &creator.pubkey()),
        collector_before + tax
    );

    // A wallet-to-wallet transfer passes free.
    let friend = w.wallet_with_sol(SOL);
    w.holdings(&friend, mint, &[friend.pubkey()]);
    let (from, to) = (
        token::holding_address(&mint, &t.pubkey()),
        token::holding_address(&mint, &friend.pubkey()),
    );
    let extras = w.env.token_hook_extras(
        &SELL_TAX,
        &mint,
        &from,
        &to,
        &t.pubkey(),
        &t.pubkey(),
        &friend.pubkey(),
    );
    let send = token::transfer(t.pubkey(), from, to, mint, Some(SELL_TAX), extras, 1_000);
    w.env.send_paid_by(&[send], &t, &[]).ok();
    assert_eq!(w.env.holding(&mint, &friend.pubkey()), 1_000);

    // The collector's own sells pass free.
    let tx = w.sell(&creator, &mint, w.env.holding(&mint, &creator.pubkey()) / 2);
    tx.ok();
    assert_eq!(tx.event::<Swapped>().cuts_in, 0);
}
