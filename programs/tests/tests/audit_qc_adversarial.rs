//! Independent adversarial audit of the launchpad's SOL cut (`docs/quote-cut.md`).
//!
//! Tests named `finding_*` DEMONSTRATE a weakness: they pass while the weakness exists (each
//! asserts the bad outcome and says so). Tests named `holds_*` pin down a property the audit
//! checked and found sound.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::{AccountSerialize, AnchorDeserialize, Discriminator};
use bordrless_hook::token_flags;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::QuoteCutTaken;
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{LaunchRules, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{SOL, VQ};
use bordrless_program_tests::vault::*;
use bordrless_swap::events::Swapped;
use bordrless_token::client as token;
use hook_tester::client as tester;
use hook_tester::quote_mode;
use hook_vault::client as vault_client;
use hook_vault::events::SlotSold;
use solana_keypair::Keypair;
use solana_signer::Signer;

// ---- helpers ---------------------------------------------------------------------------------------

fn b64(s: &str) -> Vec<u8> {
    let table = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= table(*c) << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// `QuoteCutTaken` events (logged with `emit!`).
fn cuts_taken(tx: &Tx) -> Vec<QuoteCutTaken> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .map(b64)
        .filter(|d| d.len() >= 8 && d[..8] == *QuoteCutTaken::DISCRIMINATOR)
        .map(|d| QuoteCutTaken::deserialize(&mut &d[8..]).expect("event decodes"))
        .collect()
}

fn rate(bps: u16) -> Vec<u8> {
    bps.to_le_bytes().to_vec()
}

struct Coin {
    creator: Keypair,
    mint: Pubkey,
}

/// A `hook_tester` coin from a `create_quote_cut_config` config with `cut`, creator fee
/// `creator_fee_bps`. No warp: the caller decides whether to leave the sniper window.
fn tester_coin(w: &mut World, cut: QuoteCut, creator_fee_bps: u16) -> Coin {
    let creator = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[
                tester::init_script(creator.pubkey(), mint, vec![]),
                tester::init_quote_state(creator.pubkey(), mint),
            ],
            &creator,
            &[],
        )
        .ok();
    let config = Keypair::new();
    let args = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps,
        custom_hook: Some(hook_tester::ID),
        custom_hook_flags: token_flags::BEFORE_TRANSFER,
        label: "audit".to_string(),
    };
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, cut);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "AUD", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    w.holdings(&creator, mint, &[creator.pubkey()]);
    Coin { creator, mint }
}

fn script(w: &mut World, c: &Coin, mode: u8, data: Vec<u8>) {
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                c.creator.pubkey(),
                c.mint,
                mode,
                data,
            )],
            &c.creator,
            &[],
        )
        .ok();
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

const BOTH_10: QuoteCut = QuoteCut {
    max_buy_bps: 1_000,
    max_sell_bps: 1_000,
};

// ---- F1: the cut is decided on holding(mint, actor), not on the holding the trade moves ------------

/// FINDING (medium, design): a balance-based policy (the doc's own "whale" example) reads the
/// actor's holding, but the DEX lets the tokens move from / to another holding:
///
/// - a sell signed by a DELEGATE of the whale's holding: the hook reads the delegate's (empty)
///   holding, answers 0, and the whale's tokens are sold with no SOL cut, the SOL delivered to
///   the whale;
/// - a buy signed by a fresh wallet with `recipient` = the whale: no cut, the whale's balance grows.
///
/// The doc's "a whale can't pass a smaller wallet's holding" is true only of the account list.
#[test]
fn finding_a_whale_line_is_skipped_by_a_delegate_sell_or_a_routed_buy() {
    let mut w = World::new();
    let c = tester_coin(&mut w, BOTH_10, 100);
    w.env.warp(31);
    let line: u64 = 10_000_000 * 1_000_000;
    let mut data = line.to_le_bytes().to_vec();
    data.extend(rate(700));
    script(&mut w, &c, quote_mode::WHALE, data);
    let keys = w.launch_keys(&c.mint);
    let pool = w.launch_pool_key(&c.mint);

    let whale = trader(&mut w, &c.mint, 100 * SOL);
    let ix = w.launch_swap_ix(&whale.pubkey(), &c.mint, 1, 20 * SOL, 0);
    w.env.send_paid_by(&[ix], &whale, &[]).ok();
    let held = w.env.holding(&c.mint, &whale.pubkey());
    assert!(held >= line, "the whale is above the line");

    // Control: the whale's own sell pays 7%.
    let ix = w.launch_swap_ix(&whale.pubkey(), &c.mint, 0, held / 20, 0);
    let tx = w.env.send_paid_by(&[ix], &whale, &[]);
    tx.ok();
    assert_eq!(
        cuts_taken(&tx)[0].bps,
        700,
        "control: the whale's own sell is taxed"
    );

    // 1. A delegate sells the whale's tokens.
    let delegate = w.env.funded(SOL);
    let whale_holding = token::holding_address(&c.mint, &whale.pubkey());
    let amount = held / 20;
    w.env
        .send_paid_by(
            &[token::approve(
                whale.pubkey(),
                whale_holding,
                delegate.pubkey(),
                amount,
            )],
            &whale,
            &[],
        )
        .ok();
    let vault = bordrless_swap::client::vault_address(&pool, &c.mint);
    let slice = w.env.token_hook_slice(
        &c.mint,
        &whale_holding,
        &vault,
        &delegate.pubkey(),
        &whale.pubkey(),
        &pool,
    );
    // trader = the delegate, SOL delivered to the whale (recipient = whale) ...
    let mut ix = launch::swap_with_base_slice(
        &keys,
        delegate.pubkey(),
        whale.pubkey(),
        0,
        amount,
        0,
        slice,
    );
    // ... and the input holding is the whale's (the client derives the trader's: swap it).
    let at = ix
        .accounts
        .iter()
        .position(|m| m.pubkey == token::holding_address(&c.mint, &delegate.pubkey()))
        .expect("trader_base");
    ix.accounts[at].pubkey = whale_holding;
    let ix = launch::with_quote_cut(ix, &hook_tester::ID, &c.mint, &delegate.pubkey());
    let whale_sol = w.env.holding(&w.sol, &whale.pubkey());
    let tx = w.env.send_paid_by(&[ix], &delegate, &[]);
    tx.ok();
    assert!(
        cuts_taken(&tx).is_empty(),
        "FINDING: the whale's tokens were sold with no SOL cut"
    );
    assert!(
        w.env.holding(&w.sol, &whale.pubkey()) > whale_sol,
        "the whale got the SOL"
    );
    assert_eq!(
        w.env.holding(&c.mint, &whale.pubkey()),
        held - held / 20 - amount,
        "from the whale's own holding"
    );

    // 2. A fresh wallet buys for the whale.
    let runner = w.wallet_with_sol(10 * SOL);
    let before = w.env.holding(&c.mint, &whale.pubkey());
    let slice = w.launch_base_slice(&c.mint, &runner.pubkey(), &whale.pubkey(), true);
    let ix =
        launch::swap_with_base_slice(&keys, runner.pubkey(), whale.pubkey(), 1, 5 * SOL, 0, slice);
    let ix = launch::with_quote_cut(ix, &hook_tester::ID, &c.mint, &runner.pubkey());
    let tx = w.env.send_paid_by(&[ix], &runner, &[]);
    tx.ok();
    assert!(
        cuts_taken(&tx).is_empty(),
        "FINDING: a routed buy for the whale pays no cut"
    );
    assert!(w.env.holding(&c.mint, &whale.pubkey()) > before);
}

// ---- F2: a SOL-cut config accepts any hook, also one with no `quote_cut` ---------------------------

/// FINDING (low): `create_quote_cut_config` does not check that the hook answers `quote_cut`.
/// A coin launched from such a config (here `tax_hook`) can be traded only by its creator (and
/// its hook-vault slots): every other buy or sell fails in the hook (Anchor's
/// `InstructionFallbackNotFound`, 101), with or without the four accounts.
#[test]
fn finding_a_sol_cut_on_a_hook_without_quote_cut_bricks_everyone_but_the_creator() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[tax_prepare(
                creator.pubkey(),
                mint,
                Pubkey::new_unique(),
                0,
                0,
            )],
            &creator,
            &[],
        )
        .ok();
    let config = Keypair::new();
    let args = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(tax_hook::ID),
        custom_hook_flags: tax_hook::FLAGS,
        label: "no quote_cut".to_string(),
    };
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, BOTH_10);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "TAX", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert!(
        w.launch(&mint).quote_cut().is_some(),
        "accepted at config and at launch"
    );
    w.env.warp(31);

    // Anyone else: refused by the hook, the four accounts passed as the registry lists them.
    let t = trader(&mut w, &mint, 5 * SOL);
    let ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, SOL, 0);
    assert!(
        ix.accounts
            .iter()
            .filter(|m| m.pubkey == tax_hook::ID)
            .count()
            >= 2
    );
    w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(anchor_lang::error::ErrorCode::InstructionFallbackNotFound.into());

    // The creator: lands (exempt, the hook is never asked).
    w.holdings(&creator, mint, &[creator.pubkey()]);
    let ix = w.launch_swap_ix(&creator.pubkey(), &mint, 1, SOL, 0);
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
}

// ---- F3: hook_vault's buy slot can target a SOL-cut coin and then never buys -----------------------

/// FINDING (low): `hook_vault` (unchanged on mainnet) accepts a `SellBuyBurn` slot whose target
/// is a SOL-cut coin. Its sells keep landing (the slot is its own coin's, exempt), but every
/// `execute_buy` fails with `QuoteCutAccountsMissing` (another coin's slot is not exempt and its
/// swap passes four extras), so the SOL its sells raise only piles up in `pending_sol`. It can
/// leave only by `retire` (to the incinerator), and only once the slot has been idle 60 days,
/// which its own sells keep resetting.
#[test]
fn finding_a_hook_vault_buy_slot_on_a_sol_cut_coin_sells_but_never_buys() {
    let mut w = World::new();
    let x = tester_coin(&mut w, BOTH_10, 100);
    w.env.warp(31);
    script(&mut w, &x, quote_mode::RETURN, rate(500));
    let x_pool = w.launch_pool_key(&x.mint);

    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x.mint], true);
    let m = c.mint;
    w.churn(&m, 5, 2 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let sold = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert!(sold.pending_sol > 0);

    for _ in 0..3 {
        let tx = w.execute_buy(&cranker, &m, 0);
        tx.expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
        w.env.warp(61);
    }
    assert_eq!(
        w.slot_sol(&m, 0),
        sold.pending_sol,
        "FINDING: the SOL stays stranded"
    );
    // The slot is another coin's: it never passes as `x`'s vault slot.
    assert_ne!(
        vault_client::slot_owner(&m, 0),
        vault_client::slot_owner(&x.mint, 0)
    );
}

// ---- F4: the creator's exemption follows the signer only ------------------------------------------

/// FINDING (low / informational): the exemption is `actor == launch.creator`, whoever gets the
/// tokens. The creator can buy for anyone (`recipient`) with no SOL cut, so a creator, or a
/// program whose PDA is the creator, can relay cut-free trades: Bordrless's quarter of the cut and
/// a listed config author's share of it are skipped with it. A normal trader can't borrow the
/// exemption: naming the creator as the recipient changes nothing.
#[test]
fn holds_the_creator_pays_the_cut_when_it_buys_for_someone_else() {
    let mut w = World::new();
    let c = tester_coin(&mut w, BOTH_10, 100);
    w.env.warp(31);
    script(&mut w, &c, quote_mode::RETURN, rate(1_000));
    let keys = w.launch_keys(&c.mint);
    let friend = trader(&mut w, &c.mint, SOL);
    let before = w.env.holding(&c.mint, &friend.pubkey());
    let slice = w.launch_base_slice(&c.mint, &c.creator.pubkey(), &friend.pubkey(), true);
    // Fixed after this audit: the exemption is the creator trading to itself only. Delivering to
    // someone else needs the accounts and pays the cut.
    let ix = launch::swap_with_base_slice(
        &keys,
        c.creator.pubkey(),
        friend.pubkey(),
        1,
        SOL,
        0,
        slice.clone(),
    );
    w.env
        .send_paid_by(&[ix], &c.creator, &[])
        .expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
    let ix =
        launch::swap_with_base_slice(&keys, c.creator.pubkey(), friend.pubkey(), 1, SOL, 0, slice);
    let ix = launch::with_quote_cut(ix, &hook_tester::ID, &c.mint, &c.creator.pubkey());
    let tx = w.env.send_paid_by(&[ix], &c.creator, &[]);
    tx.ok();
    assert_eq!(
        cuts_taken(&tx)[0].bps,
        1_000,
        "a buy delivered to someone else pays"
    );
    assert!(w.env.holding(&c.mint, &friend.pubkey()) > before);

    // A trader naming the creator as recipient, without the accounts: refused.
    let t = trader(&mut w, &c.mint, 5 * SOL);
    let slice = w.launch_base_slice(&c.mint, &t.pubkey(), &c.creator.pubkey(), true);
    let ix = launch::swap_with_base_slice(&keys, t.pubkey(), c.creator.pubkey(), 1, SOL, 0, slice);
    w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
    // With them: it pays, though the creator gets the tokens.
    let slice = w.launch_base_slice(&c.mint, &t.pubkey(), &c.creator.pubkey(), true);
    let ix =
        launch::swap_with_base_slice(&keys, t.pubkey(), c.creator.pubkey(), 1, SOL / 2, 0, slice);
    let ix = launch::with_quote_cut(ix, &hook_tester::ID, &c.mint, &t.pubkey());
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    assert_eq!(cuts_taken(&tx)[0].bps, 1_000);
}

// ---- properties checked and found sound ------------------------------------------------------------

/// A hook-vault slot of the coin itself trades without the four accounts and is never asked
/// (and the hook's state is untouched), while a fresh wallet without them is refused.
#[test]
fn holds_only_the_coins_own_vault_slots_and_creator_skip_the_cut() {
    let mut w = World::new();
    // A tax_hook coin with a vault whose slot 0 sells for SOL, from a SOL-cut config: the cut is on
    // tax_hook (which has no `quote_cut`), so any call to it would fail the trade.
    let creator = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let to = w.env.funded(SOL);
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &mint,
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &CoinSpec::default(),
    );
    w.env.send_paid_by(&setup, &creator, &[&mint_kp]).ok();
    let config = Keypair::new();
    let args = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(tax_hook::ID),
        custom_hook_flags: tax_hook::FLAGS,
        label: "vaulted cut".to_string(),
    };
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, BOTH_10);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "VLT", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    w.open_vault(&creator, &mint).ok();
    w.env.warp(61);

    // The creator buys and funds slot 0 (tax_hook's collector).
    w.holdings(&creator, mint, &[creator.pubkey()]);
    let ix = w.launch_swap_ix(&creator.pubkey(), &mint, 1, 5 * SOL, 0);
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    let half = w.env.holding(&mint, &creator.pubkey()) / 2;
    w.send_tokens(&creator, mint, &vault_client::slot_owner(&mint, 0), half)
        .ok();
    // The slot sells: exempt, lands.
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.execute(&cranker, &mint, 0);
    tx.ok();
    assert!(tx.event::<SlotSold>().got > 0);
    // Someone else without the accounts: refused before the hook.
    let t = trader(&mut w, &mint, 2 * SOL);
    let keys = w.launch_keys(&mint);
    let slice = w.launch_base_slice(&mint, &t.pubkey(), &t.pubkey(), true);
    let ix = launch::swap_with_base_slice(&keys, t.pubkey(), t.pubkey(), 1, SOL, 0, slice);
    w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
}

/// The worst fee stack the protocol bounds allow (creator fee 10%, sniper LP fee 90%, the hook's
/// 10% cut, Bordrless's quarter of the cuts) still leaves a buy something to swap: the cut never
/// makes a real-sized buy fail with `FeesExceedInput`. Outside the sniper window, dust buys and
/// sells land or fail exactly as they would with no cut (the cut rounds down; the drop rule keeps
/// the deltas below the amount). The one exception, found by brute force over the same formulas
/// and not exercised here: inside the sniper window (LP fee 80 to 90%) a buy of at most 62
/// lamports can flip from landing to `FeesExceedInput`, because the drop rule ignores the LP fee
/// and Bordrless's share; immaterial.
#[test]
fn holds_the_cut_never_bricks_a_buy_that_would_land_without_it() {
    let mut w = World::new();
    // Raise the launchpad config to the protocol's ceilings (creator fee and rules total 1,000,
    // sniper 9,000).
    let key = launch::config_address();
    let mut cfg: bordrless_launch::state::Config = w.env.read(&key);
    cfg.max_creator_fee_bps = 1_000;
    cfg.rule_bounds.max_rules_fee_bps = 1_000;
    cfg.sniper_start_bps = bordrless_swap::constants::MAX_LP_FEE_BPS;
    let mut account = w.env.account(&key).expect("config");
    let mut data = Vec::new();
    cfg.try_serialize(&mut data).expect("serialize");
    account.data[..data.len()].copy_from_slice(&data);
    w.env.put(key, account);

    // Two coins launched alike: `a`'s hook answers 0, `b`'s 10%.
    let a = tester_coin(&mut w, BOTH_10, 1_000);
    let b = tester_coin(&mut w, BOTH_10, 1_000);
    script(&mut w, &b, quote_mode::RETURN, rate(1_000));
    // Inside the sniper window, at its top, on `b`.
    let t = trader(&mut w, &b.mint, 50 * SOL);
    w.holdings(&t, a.mint, &[t.pubkey()]);
    let ix = w.launch_swap_ix(&t.pubkey(), &b.mint, 1, SOL, 0);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    let ev = tx.event::<Swapped>();
    assert!(ev.lp_fee_bps >= 8_900, "in the window: {}", ev.lp_fee_bps);
    assert_eq!(cuts_taken(&tx)[0].bps, 1_000);
    let ix = w.launch_swap_ix(&t.pubkey(), &a.mint, 1, SOL, 0);
    w.env.send_paid_by(&[ix], &t, &[]).ok();

    // Dust buys and sells: the same outcome with a 10% cut as with none. A sell's size is scaled
    // so that its SOL out is about `amount` lamports.
    w.env.warp(31);
    let per_lamport = (w.env.holding(&a.mint, &t.pubkey()) / SOL).max(1);
    for amount in [
        1u64, 2, 3, 4, 5, 9, 10, 11, 19, 20, 21, 99, 100, 101, 1_000, 10_000,
    ] {
        for buy in [true, false] {
            let outcome: Vec<String> = [&a, &b]
                .iter()
                .map(|c| {
                    let size = if buy { amount } else { amount * per_lamport };
                    let ix = w.launch_swap_ix(&t.pubkey(), &c.mint, u8::from(buy), size, 0);
                    let tx = w.env.send_paid_by(&[ix], &t, &[]);
                    match &tx.result {
                        Ok(_) => "ok".to_string(),
                        Err(f) => format!("{:?}", f.err),
                    }
                })
                .collect();
            assert_eq!(
                outcome[0],
                outcome[1],
                "{} of {amount}: without the cut {}, with it {}",
                if buy { "buy" } else { "sell" },
                outcome[0],
                outcome[1]
            );
        }
    }
}

/// A caller can't hand the launchpad a writable account the hook may write but the state: the
/// launchpad passes the hook every account read-only (the signer as a signer) but the state, as
/// the test hook sees it. Checked through the registry's own resolution of the four accounts.
#[test]
fn holds_the_registry_marks_only_the_state_writable() {
    let mut w = World::new();
    let c = tester_coin(&mut w, BOTH_10, 100);
    let extras = launch::quote_cut_extras(&hook_tester::ID, &c.mint, &Pubkey::new_unique());
    let writable: Vec<bool> = extras.iter().map(|m: &AccountMeta| m.is_writable).collect();
    assert_eq!(writable, vec![false, false, true, false]);
    assert!(extras.iter().all(|m| !m.is_signer));
}
