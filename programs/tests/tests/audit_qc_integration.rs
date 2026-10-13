//! Integration audit of the SOL-cut launchpad upgrade (`docs/quote-cut.md`): live-state safety and
//! the programs that trade on launch pools.
//!
//! - An existing launch, made by the mainnet launchpad, trades byte for byte as before under the new
//!   binary (the same events, the same `Launch` and pool afterwards), with or without accounts after
//!   the pool's four extras; the CU of each path is printed old against new. Needs the mainnet
//!   build: `AUDIT_OLD_LAUNCH_SO=<path to the mainnet bordrless_launch.so>` (the test says so and
//!   passes when it is unset).
//! - A `hook_vault` slot sells a SOL-cut coin through the vault's own swap (the pool's four extras
//!   only): the slot exemption end to end. The coin's hook here is `tax_hook`, which has no
//!   `quote_cut`, and that shows the other half: any SOL-cut config may name a hook without
//!   `quote_cut`, and then every trade but the creator's and the vault's on the capped side fails
//!   (a honeypot built from a protocol hook).
//! - A `SellBuyBurn` slot may target a SOL-cut coin (`create_vault` takes it), and then can never
//!   buy: its SOL is stuck until the slot retires.
//! - The cost of a SOL-cut launch: CU, the pool registry's size and rent.
//! - Error ordinals: the four new codes are appended after the last old one.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::Discriminator;
use bordrless_hook::{token_flags, HookAccountList};
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{LaunchCreated, LaunchQuoteCut, QuoteCutConfigured, QuoteCutTaken};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{LaunchRules, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::vault::*;
use hook_tester::client as tester;
use hook_tester::quote_mode;
use hook_vault::events::SlotSold;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Anchor's `InstructionFallbackNotFound`: what a hook without `quote_cut` answers the launchpad.
const FALLBACK_NOT_FOUND: u32 = 101;

fn config_args(hook: Pubkey, flags: u16, label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(hook),
        custom_hook_flags: flags,
        label: label.to_string(),
    }
}

fn cut_config(w: &mut World, creator: &Keypair, args: CreateConfigArgs, cut: QuoteCut) -> Pubkey {
    let config = Keypair::new();
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, cut);
    w.env.send_paid_by(&[ix], creator, &[&config]).ok();
    config.pubkey()
}

/// A coin on hook_tester from `config` (its token hook script and its quote-cut state made first).
fn tester_coin(w: &mut World, creator: &Keypair, config: &Pubkey, symbol: &str) -> (Pubkey, Tx) {
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[
                tester::init_script(creator.pubkey(), mint, vec![]),
                tester::init_quote_state(creator.pubkey(), mint),
            ],
            creator,
            &[],
        )
        .ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, symbol, VQ, config);
    let tx = w.env.send_paid_by(&[ix], creator, &[&mint_kp]);
    (mint, tx)
}

// ---- 1. an existing launch under the new binary ------------------------------------------------

fn set_launchpad(w: &mut World, bytes: &[u8]) {
    w.env
        .svm
        .add_program(bordrless_launch::ID, bytes)
        .expect("load the launchpad");
    let deployer = w.env.deployer.pubkey();
    w.env
        .set_upgrade_authority(bordrless_launch::ID, Some(deployer));
}

/// What a trade did: its events (program, discriminator, body) and the launch's and pool's bytes.
type Effect = (Vec<(Pubkey, [u8; 8], Vec<u8>)>, Vec<u8>, Vec<u8>);

fn effect(w: &World, tx: &Tx, mint: &Pubkey) -> Effect {
    let events = tx
        .raw_events()
        .into_iter()
        .map(|e| (e.program, e.discriminator, e.body))
        .collect();
    let l = w.env.account(&launch::launch_address(mint)).unwrap().data;
    let p = w.env.account(&w.launch_pool_key(mint)).unwrap().data;
    (events, l, p)
}

/// Sends `ix` under the old launchpad and, from the same state, under the new one; asserts both land
/// with the same effect; leaves the new binary's state, then puts the old binary back.
fn same_under_both(
    w: &mut World,
    old: &[u8],
    new: &[u8],
    what: &str,
    mint: &Pubkey,
    ix: &anchor_lang::solana_program::instruction::Instruction,
    signer: &Keypair,
) {
    let snapshot = w.env.svm.clone();
    let tx_old = w.env.send_paid_by(std::slice::from_ref(ix), signer, &[]);
    tx_old.ok();
    let e_old = effect(w, &tx_old, mint);
    w.env.svm = snapshot;
    set_launchpad(w, new);
    let tx_new = w.env.send_paid_by(std::slice::from_ref(ix), signer, &[]);
    tx_new.ok();
    let e_new = effect(w, &tx_new, mint);
    assert_eq!(
        e_old, e_new,
        "{what}: the new binary changed what the trade did"
    );
    assert_eq!(tx_old.inner_len(), tx_new.inner_len(), "{what}: the CPIs");
    println!(
        "{what}: CU old {} new {} ({:+})",
        tx_old.cu(),
        tx_new.cu(),
        tx_new.cu() as i64 - tx_old.cu() as i64
    );
    set_launchpad(w, old);
}

#[test]
fn an_existing_launch_trades_exactly_as_before_under_the_new_binary() {
    let Some(path) = std::env::var_os("AUDIT_OLD_LAUNCH_SO") else {
        println!(
            "AUDIT_OLD_LAUNCH_SO is unset: skipped (point it at the mainnet bordrless_launch.so)"
        );
        return;
    };
    let old = std::fs::read(path).expect("the mainnet launchpad");
    let new = bordrless_program_tests::program_bytes("bordrless_launch");
    assert_ne!(old, new);
    let mut w = World::new();
    set_launchpad(&mut w, &old);

    // Launches made by the old binary: no rules; kit rules with holder fees and burns both ways;
    // a custom hook from a config made the old way.
    let creator = w.wallet_with_sol(50 * SOL);
    let (plain, tx) = w.create_launch_with(&creator, "OLD0", 100, VQ, LaunchRules::NONE);
    tx.ok();
    let kit_rules = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        burn_buy_bps: 50,
        burn_sell_bps: 50,
        ..LaunchRules::NONE
    };
    let (kit, tx) = w.create_launch_with(&creator, "OLD1", 100, VQ, kit_rules);
    tx.ok();
    let (config, tx) = w.create_config(
        &creator,
        config_args(hook_tester::ID, token_flags::BEFORE_TRANSFER, "Old"),
    );
    tx.ok();
    let (custom, tx) = tester_coin(&mut w, &creator, &config, "OLD2");
    tx.ok();
    w.env.warp(31);

    for (name, mint) in [
        ("no rules", plain),
        ("kit rules", kit),
        ("custom hook", custom),
    ] {
        let l = w.launch(&mint);
        assert_eq!(l.reserved, [0; 22]);
        assert_eq!(l.quote_cut(), None);
        let reg = w.env.account(&launch::registry_address(&l.pool)).unwrap();
        assert_eq!(
            HookAccountList::decode(&reg.data).unwrap(),
            bordrless_launch::instructions::launch::registry_list(l.holder_vault, l.kit_config)
        );
        let t = w.wallet_with_sol(20 * SOL);
        w.holdings(&t, mint, &[t.pubkey()]);
        // A buy and a sell with the pool's four extras.
        let ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, 2 * SOL, 0);
        same_under_both(&mut w, &old, &new, &format!("{name}: buy"), &mint, &ix, &t);
        let held = w.env.holding(&mint, &t.pubkey());
        let ix = w.launch_swap_ix(&t.pubkey(), &mint, 0, held / 3, 0);
        same_under_both(&mut w, &old, &new, &format!("{name}: sell"), &mint, &ix, &t);
        // Four unrelated accounts after the four extras: ignored by both.
        let junk: Vec<AccountMeta> = (0..4)
            .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
            .collect();
        let mut ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, SOL, 0);
        ix.accounts.extend(junk.clone());
        same_under_both(
            &mut w,
            &old,
            &new,
            &format!("{name}: buy + 4 junk"),
            &mint,
            &ix,
            &t,
        );
        // The SOL-cut accounts a client might pass blindly (hook_tester's): ignored by both.
        let mut ix = w.launch_swap_ix(&t.pubkey(), &mint, 0, held / 3, 0);
        ix.accounts.extend(launch::quote_cut_extras(
            &hook_tester::ID,
            &mint,
            &t.pubkey(),
        ));
        same_under_both(
            &mut w,
            &old,
            &new,
            &format!("{name}: sell + cut-shaped"),
            &mint,
            &ix,
            &t,
        );
    }
    // The creator's claim, and a launch created by the old binary still decodes as before.
    set_launchpad(&mut w, &new);
    for mint in [plain, kit, custom] {
        let ix = launch::claim_creator_fees(creator.pubkey(), mint, w.sol);
        w.env.send_paid_by(&[ix], &creator, &[]).ok();
        assert_eq!(w.launch(&mint).quote_cut_accrued(), 0);
    }
}

// ---- 2. the vault-slot exemption, end to end; a hook without `quote_cut` ------------------------

#[test]
fn a_vault_slot_sells_a_sol_cut_coin_and_a_hook_without_quote_cut_blocks_every_other_sell() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    // Fixed after this audit: a sell-only cap is refused, so a hook without `quote_cut` (tax_hook,
    // a protocol hook) can no longer let anyone buy and only the creator sell.
    let refused = Keypair::new();
    let ix = launch::create_quote_cut_config(
        creator.pubkey(),
        refused.pubkey(),
        config_args(tax_hook::ID, tax_hook::FLAGS, "Trap"),
        0,
        QuoteCut {
            max_buy_bps: 0,
            max_sell_bps: 500,
        },
    );
    w.env
        .send_paid_by(&[ix], &creator, &[&refused])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::InvalidQuoteCut,
        ));
    // Buys always ask the hook: on tax_hook every buy but the creator's fails, so nobody is trapped.
    let config = cut_config(
        &mut w,
        &creator,
        config_args(tax_hook::ID, tax_hook::FLAGS, "Taxed cut"),
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    let to = w.env.funded(SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let setup = w.vault_setup_ixs(
        &creator.pubkey(),
        &mint,
        vault_args(vec![sell_for_sol(to.pubkey())]),
        &[],
        &CoinSpec::default(),
    );
    w.env.send_paid_by(&setup, &creator, &[&mint_kp]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "TRAP", VQ, &config);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    assert_eq!(tx.events::<LaunchQuoteCut>().len(), 1);
    w.open_vault(&creator, &mint).ok();
    w.env.warp(31);

    let t = w.wallet_with_sol(3 * SOL);
    let tx = w.buy(&t, &mint, 2 * SOL);
    tx.expect_code(FALLBACK_NOT_FOUND);
    println!("a buy on a SOL-cut tax_hook coin: error {}", tx.custom());
    // The creator trades to itself, exempt; slot 0 collects tax_hook's 1% of its buys.
    w.buy(&creator, &mint, 5 * SOL).ok();
    assert!(w.slot_coin(&mint, 0) > 0);
    let held = w.env.holding(&mint, &creator.pubkey());
    let tx = w.sell(&creator, &mint, held / 10);
    tx.ok();
    assert!(logged_cut(&tx).is_empty());

    // The vault's slot sells with the pool's four extras only (and the SDK's four appended for the
    // slot as the outer instruction's extras change nothing): exempt, the hook not asked.
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let mut ix = w.execute_ix(&cranker.pubkey(), &mint, 0);
    ix.accounts.extend(launch::quote_cut_extras(
        &tax_hook::ID,
        &mint,
        &hook_vault::client::slot_owner(&mint, 0),
    ));
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.ok();
    let sold = tx.event::<SlotSold>();
    assert!(sold.sold > 0 && sold.got > 0);
    assert!(logged_cut(&tx).is_empty());
    println!(
        "slot 0's sale of a SOL-cut coin: sold {} for {} lamports, CU {}, height {}",
        sold.sold,
        sold.got,
        tx.cu(),
        tx.max_height()
    );
    assert_eq!(w.launch(&mint).quote_cut_accrued(), 0);
}

fn logged_cut(tx: &Tx) -> Vec<String> {
    let disc = QuoteCutTaken::DISCRIMINATOR;
    tx.logs()
        .iter()
        .filter(|l| l.starts_with("Program data: "))
        .filter(|l| {
            let b64 = &l["Program data: ".len()..];
            // The first 8 bytes of the payload are 11 base64 characters and a bit; compare by decoding.
            decode_b64(b64).get(..8) == Some(disc)
        })
        .cloned()
        .collect()
}

fn decode_b64(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
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
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c) << (18 - 6 * i);
        }
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

// ---- 3. a SellBuyBurn slot aimed at a SOL-cut coin ------------------------------------------------

#[test]
fn a_sell_buy_burn_slot_takes_a_sol_cut_target_and_can_never_buy_it() {
    let mut w = World::new();
    let x_creator = w.wallet_with_sol(20 * SOL);
    let x_config = cut_config(
        &mut w,
        &x_creator,
        config_args(hook_tester::ID, token_flags::BEFORE_TRANSFER, "X cut"),
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    let (x, tx) = tester_coin(&mut w, &x_creator, &x_config, "XCUT");
    tx.ok();
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                x_creator.pubkey(),
                x,
                quote_mode::RETURN,
                100u16.to_le_bytes().to_vec(),
            )],
            &x_creator,
            &[],
        )
        .ok();
    let x_pool = w.launch_pool_key(&x);
    // `create_vault` and `open_vault` take it: nothing reads the target's SOL cut.
    let c = w.vault_coin(vault_args(vec![sell_buy_burn(x_pool)]), &[x], true);
    let m = c.mint;
    w.churn(&m, 5, 2 * SOL);
    w.env.warp(61);
    let cranker = w.wallet_with_sol(SOL);
    let sold = w.execute(&cranker, &m, 0).event::<SlotSold>();
    assert!(sold.pending_sol > 0);
    // The buy: the vault's swap passes the pool's four extras, and the slot is not X's.
    let tx = w.execute_buy(&cranker, &m, 0);
    tx.expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
    // Appending the four for the slot to the outer instruction does not help: the vault builds the
    // swap itself.
    let mut ix = w.execute_buy_ix(&cranker.pubkey(), &m, 0);
    ix.accounts.extend(launch::quote_cut_extras(
        &hook_tester::ID,
        &x,
        &hook_vault::client::slot_owner(&m, 0),
    ));
    w.env
        .send_paid_by(&[ix], &cranker, &[])
        .expect_code(u32::from(LaunchError::QuoteCutAccountsMissing));
    println!(
        "a SellBuyBurn slot aimed at a SOL-cut coin holds {} lamports it can never spend",
        w.slot_sol(&m, 0)
    );
}

// ---- 4. the cost of a SOL-cut launch ---------------------------------------------------------------

#[test]
fn a_sol_cut_launch_costs_a_larger_registry_and_a_little_compute() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(50 * SOL);
    let (plain, tx) = w.create_config(
        &creator,
        config_args(hook_tester::ID, token_flags::BEFORE_TRANSFER, "Plain"),
    );
    tx.ok();
    let cut = cut_config(
        &mut w,
        &creator,
        config_args(hook_tester::ID, token_flags::BEFORE_TRANSFER, "Cut"),
        QuoteCut {
            max_buy_bps: 1_000,
            max_sell_bps: 1_000,
        },
    );
    let mut sizes = Vec::new();
    for (name, config) in [("plain", plain), ("cut", cut)] {
        let before = w.env.lamports(&creator.pubkey());
        let (mint, tx) = tester_coin(&mut w, &creator, &config, "COST");
        tx.ok();
        let reg = w
            .env
            .account(&launch::registry_address(&w.launch_pool_key(&mint)))
            .unwrap();
        println!(
            "create_launch from a {name} config: CU {}, {} bytes, height {}, registry {} bytes ({} lamports), creator paid {}",
            tx.cu(),
            tx.size,
            tx.max_height(),
            reg.data.len(),
            reg.lamports,
            before - w.env.lamports(&creator.pubkey())
        );
        assert!(tx.cu() < 600_000);
        sizes.push(reg.data.len());
    }
    assert_eq!(sizes[1] - sizes[0], 192, "four more extras");
}

// ---- 4b. a listed SOL-cut config: the author's share covers the cut ----------------------------

#[test]
fn a_listed_sol_cut_configs_author_takes_its_share_of_the_cut_too() {
    let mut w = World::new();
    let author = w.wallet_with_sol(10 * SOL);
    let config = Keypair::new();
    let ix = launch::create_quote_cut_config(
        author.pubkey(),
        config.pubkey(),
        config_args(hook_tester::ID, token_flags::BEFORE_TRANSFER, "Listed cut"),
        2_000,
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    w.env.send_paid_by(&[ix], &author, &[&config]).ok();
    let config = config.pubkey();
    w.holdings(&author, w.sol, &[author.pubkey()]);
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = tester_coin(&mut w, &creator, &config, "LCUT");
    tx.ok();
    assert_eq!(w.launch(&mint).author_share_bps, 2_000);
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                creator.pubkey(),
                mint,
                quote_mode::RETURN,
                400u16.to_le_bytes().to_vec(),
            )],
            &creator,
            &[],
        )
        .ok();
    w.env.warp(31);
    let t = w.wallet_with_sol(20 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let ix = w.launch_swap_ix(&t.pubkey(), &mint, 1, 5 * SOL, 0);
    w.env.send_paid_by(&[ix], &t, &[]).ok();
    let held = w.env.holding(&mint, &t.pubkey());
    let ix = w.launch_swap_ix(&t.pubkey(), &mint, 0, held / 2, 0);
    w.env.send_paid_by(&[ix], &t, &[]).ok();
    let l = w.launch(&mint);
    let cut = l.quote_cut_accrued();
    assert!(cut > 0);
    let pot = w.env.holding(&w.sol, &launch::launch_address(&mint));
    assert_eq!(
        pot, l.creator_fees_accrued,
        "the holding holds the creator fee and the cut"
    );
    let (a0, c0) = (
        w.env.holding(&w.sol, &author.pubkey()),
        w.env.holding(&w.sol, &creator.pubkey()),
    );
    let ix =
        launch::claim_creator_fees_shared(creator.pubkey(), mint, w.sol, config, author.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[]).ok();
    let (a1, c1) = (
        w.env.holding(&w.sol, &author.pubkey()),
        w.env.holding(&w.sol, &creator.pubkey()),
    );
    assert_eq!(
        a1 - a0,
        pot * 2_000 / 10_000,
        "the author's 20% of all of it, the cut included"
    );
    assert_eq!(c1 - c0, pot - (a1 - a0));
    println!(
        "listed SOL-cut config: claimed {pot} (cut {cut}): author {}, creator {}",
        a1 - a0,
        c1 - c0
    );
}

// ---- 5. ordinals ---------------------------------------------------------------------------------

#[test]
fn the_new_error_codes_follow_the_last_old_one() {
    assert_eq!(u32::from(LaunchError::HookTimelockPending), 6045);
    assert_eq!(u32::from(LaunchError::InvalidQuoteCut), 6046);
    assert_eq!(u32::from(LaunchError::QuoteCutAccountsMissing), 6047);
    assert_eq!(u32::from(LaunchError::QuoteCutWrongAccount), 6048);
    assert_eq!(u32::from(LaunchError::QuoteCutBadAnswer), 6049);
    let discs = [
        LaunchCreated::DISCRIMINATOR,
        LaunchQuoteCut::DISCRIMINATOR,
        QuoteCutConfigured::DISCRIMINATOR,
        QuoteCutTaken::DISCRIMINATOR,
    ];
    for (i, a) in discs.iter().enumerate() {
        for b in &discs[i + 1..] {
            assert_ne!(a, b);
        }
    }
}
